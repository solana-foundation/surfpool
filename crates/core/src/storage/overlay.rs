use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};

use super::{OverlayDelta, OverlayLike, Storage, StorageError, StorageResult};

/// Represents the state of a key in the overlay
#[derive(Clone)]
enum OverlayEntry<V> {
    /// Value was written to overlay
    Written(V),
    /// Value was deleted in overlay (tombstone)
    Deleted,
}

/// Thread-safe overlay storage that wraps a base storage.
/// All writes go to an in-memory HashMap overlay.
/// Reads check overlay first, then fall through to base.
/// Deletes are tracked as tombstones in the overlay.
///
/// This is useful for transaction profiling where we need to
/// read from the database but not persist any mutations.
pub struct OverlayStorage<K, V> {
    /// The base storage (could be SQLite, Postgres, HashMap, etc.)
    base: Box<dyn Storage<K, V>>,
    /// In-memory overlay for writes and deletes
    /// Using Arc<RwLock<_>> for thread-safety (Send + Sync)
    overlay: Arc<RwLock<HashMap<K, OverlayEntry<V>>>>,
    /// Track if base was "cleared" - if true, ignore base for reads
    base_cleared: Arc<RwLock<bool>>,
}

impl<K, V> OverlayStorage<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Create a new overlay wrapping the given base storage
    pub fn new(base: Box<dyn Storage<K, V>>) -> Self {
        Self {
            base,
            overlay: Arc::new(RwLock::new(HashMap::new())),
            base_cleared: Arc::new(RwLock::new(false)),
        }
    }
}

impl<K, V> OverlayStorage<K, V>
where
    K: Serialize + for<'de> Deserialize<'de> + Clone + Eq + Hash + Send + Sync + 'static,
    V: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    /// Create a boxed overlay from a base storage.
    /// This is a convenience method for wrapping storage fields.
    pub fn wrap(base: Box<dyn Storage<K, V>>) -> Box<dyn Storage<K, V>> {
        Box::new(OverlayStorage::new(base))
    }
}

impl<K, V> Storage<K, V> for OverlayStorage<K, V>
where
    K: Serialize + for<'de> Deserialize<'de> + Clone + Eq + Hash + Send + Sync + 'static,
    V: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn store(&mut self, key: K, value: V) -> StorageResult<()> {
        // Write only to overlay, never to base
        let mut overlay = self.overlay.write().map_err(|_| StorageError::LockError)?;
        overlay.insert(key, OverlayEntry::Written(value));
        Ok(())
    }

    fn get(&self, key: &K) -> StorageResult<Option<V>> {
        // First check overlay
        let overlay = self.overlay.read().map_err(|_| StorageError::LockError)?;

        if let Some(entry) = overlay.get(key) {
            return match entry {
                OverlayEntry::Written(v) => Ok(Some(v.clone())),
                OverlayEntry::Deleted => Ok(None), // Tombstone - don't query base
            };
        }
        drop(overlay); // Release read lock before querying base

        // Check if base was cleared
        let base_cleared = self
            .base_cleared
            .read()
            .map_err(|_| StorageError::LockError)?;
        if *base_cleared {
            return Ok(None);
        }
        drop(base_cleared);

        // Fall through to base storage
        self.base.get(key)
    }

    fn take(&mut self, key: &K) -> StorageResult<Option<V>> {
        let mut overlay = self.overlay.write().map_err(|_| StorageError::LockError)?;

        // Check if key exists in overlay
        if let Some(entry) = overlay.get(key) {
            match entry {
                OverlayEntry::Written(v) => {
                    let value = v.clone();
                    // Replace with tombstone
                    overlay.insert(key.clone(), OverlayEntry::Deleted);
                    return Ok(Some(value));
                }
                OverlayEntry::Deleted => {
                    // Already deleted
                    return Ok(None);
                }
            }
        }
        drop(overlay);

        // Check if base was cleared
        let base_cleared = self
            .base_cleared
            .read()
            .map_err(|_| StorageError::LockError)?;
        if *base_cleared {
            return Ok(None);
        }
        drop(base_cleared);

        // Get from base (but don't modify base)
        let value = self.base.get(key)?;

        if value.is_some() {
            // Mark as deleted in overlay
            let mut overlay = self.overlay.write().map_err(|_| StorageError::LockError)?;
            overlay.insert(key.clone(), OverlayEntry::Deleted);
        }

        Ok(value)
    }

    fn clear(&mut self) -> StorageResult<()> {
        // Mark base as cleared and clear overlay
        let mut base_cleared = self
            .base_cleared
            .write()
            .map_err(|_| StorageError::LockError)?;
        *base_cleared = true;

        let mut overlay = self.overlay.write().map_err(|_| StorageError::LockError)?;
        overlay.clear();

        Ok(())
    }

    fn keys(&self) -> StorageResult<Vec<K>> {
        let overlay = self.overlay.read().map_err(|_| StorageError::LockError)?;
        let base_cleared = *self
            .base_cleared
            .read()
            .map_err(|_| StorageError::LockError)?;

        let mut result_keys: HashSet<K> = HashSet::new();
        let mut deleted_keys: HashSet<K> = HashSet::new();

        // Collect overlay keys (written) and deleted keys
        for (k, entry) in overlay.iter() {
            match entry {
                OverlayEntry::Written(_) => {
                    result_keys.insert(k.clone());
                }
                OverlayEntry::Deleted => {
                    deleted_keys.insert(k.clone());
                }
            }
        }

        // If base not cleared, add base keys (excluding deleted ones)
        if !base_cleared {
            drop(overlay);

            for key in self.base.keys()? {
                if !deleted_keys.contains(&key) && !result_keys.contains(&key) {
                    result_keys.insert(key);
                }
            }
        }

        Ok(result_keys.into_iter().collect())
    }

    fn into_iter(&self) -> StorageResult<Box<dyn Iterator<Item = (K, V)> + '_>> {
        let overlay = self.overlay.read().map_err(|_| StorageError::LockError)?;
        let base_cleared = *self
            .base_cleared
            .read()
            .map_err(|_| StorageError::LockError)?;

        // Collect deleted keys for filtering
        let deleted_keys: HashSet<K> = overlay
            .iter()
            .filter_map(|(k, entry)| {
                if matches!(entry, OverlayEntry::Deleted) {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();

        // Collect overlay written entries
        let overlay_entries: Vec<(K, V)> = overlay
            .iter()
            .filter_map(|(k, entry)| {
                if let OverlayEntry::Written(v) = entry {
                    Some((k.clone(), v.clone()))
                } else {
                    None
                }
            })
            .collect();

        let overlay_keys: HashSet<K> = overlay_entries.iter().map(|(k, _)| k.clone()).collect();

        drop(overlay);

        // Get base entries if not cleared
        let base_entries: Vec<(K, V)> = if !base_cleared {
            self.base
                .into_iter()?
                .filter(|(k, _)| !deleted_keys.contains(k) && !overlay_keys.contains(k))
                .collect()
        } else {
            Vec::new()
        };

        // Chain overlay entries with filtered base entries
        let all_entries = overlay_entries
            .into_iter()
            .chain(base_entries)
            .collect::<Vec<_>>();
        Ok(Box::new(all_entries.into_iter()))
    }

    fn count(&self) -> StorageResult<u64> {
        // Use keys() which handles all edge cases correctly
        Ok(self.keys()?.len() as u64)
    }

    fn shutdown(&self) {
        // No-op - don't propagate to base
        // The base storage should not be affected by overlay shutdown
    }

    fn clone_box(&self) -> Box<dyn Storage<K, V>> {
        // Clone the overlay with its current state
        let overlay = self.overlay.read().unwrap();
        let base_cleared = *self.base_cleared.read().unwrap();

        Box::new(OverlayStorage {
            base: self.base.clone_box(),
            overlay: Arc::new(RwLock::new(overlay.clone())),
            base_cleared: Arc::new(RwLock::new(base_cleared)),
        })
    }

    fn as_overlay(&self) -> Option<&dyn OverlayLike<K, V>> {
        Some(self)
    }
}

impl<K, V> OverlayLike<K, V> for OverlayStorage<K, V>
where
    K: Serialize + for<'de> Deserialize<'de> + Clone + Eq + Hash + Send + Sync + 'static,
    V: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn extract_overlay(&self) -> StorageResult<OverlayDelta<K, V>> {
        let overlay = self.overlay.read().map_err(|_| StorageError::LockError)?;
        let base_cleared = *self
            .base_cleared
            .read()
            .map_err(|_| StorageError::LockError)?;

        let mut writes = Vec::new();
        let mut deletes = Vec::new();
        for (k, entry) in overlay.iter() {
            match entry {
                OverlayEntry::Written(v) => writes.push((k.clone(), v.clone())),
                OverlayEntry::Deleted => deletes.push(k.clone()),
            }
        }

        Ok(OverlayDelta {
            writes,
            deletes,
            base_cleared,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageHashMap;

    #[test]
    fn test_overlay_write_does_not_affect_base() {
        let mut base: Box<dyn Storage<String, String>> =
            Box::new(StorageHashMap::<String, String>::new());
        base.store("key1".into(), "base_value".into()).unwrap();

        let mut overlay = OverlayStorage::new(base.clone_box());
        overlay
            .store("key1".into(), "overlay_value".into())
            .unwrap();

        // Overlay should return overlay value
        assert_eq!(
            overlay.get(&"key1".into()).unwrap(),
            Some("overlay_value".into())
        );

        // Base should still have original value
        assert_eq!(base.get(&"key1".into()).unwrap(), Some("base_value".into()));
    }

    #[test]
    fn test_overlay_read_falls_through_to_base() {
        let mut base: Box<dyn Storage<String, String>> =
            Box::new(StorageHashMap::<String, String>::new());
        base.store("key1".into(), "base_value".into()).unwrap();

        let overlay = OverlayStorage::new(base);

        assert_eq!(
            overlay.get(&"key1".into()).unwrap(),
            Some("base_value".into())
        );
    }

    #[test]
    fn test_overlay_delete_creates_tombstone() {
        let mut base: Box<dyn Storage<String, String>> =
            Box::new(StorageHashMap::<String, String>::new());
        base.store("key1".into(), "base_value".into()).unwrap();

        let mut overlay = OverlayStorage::new(base.clone_box());
        let taken = overlay.take(&"key1".into()).unwrap();

        assert_eq!(taken, Some("base_value".into()));
        assert_eq!(overlay.get(&"key1".into()).unwrap(), None);

        // Base should still have the value
        assert_eq!(base.get(&"key1".into()).unwrap(), Some("base_value".into()));
    }

    #[test]
    fn test_overlay_keys_merges_correctly() {
        let mut base: Box<dyn Storage<String, String>> =
            Box::new(StorageHashMap::<String, String>::new());
        base.store("base_key".into(), "value".into()).unwrap();

        let mut overlay = OverlayStorage::new(base);
        overlay.store("overlay_key".into(), "value".into()).unwrap();

        let keys = overlay.keys().unwrap();
        assert!(keys.contains(&"base_key".into()));
        assert!(keys.contains(&"overlay_key".into()));
    }

    #[test]
    fn test_overlay_clear_ignores_base() {
        let mut base: Box<dyn Storage<String, String>> =
            Box::new(StorageHashMap::<String, String>::new());
        base.store("key1".into(), "base_value".into()).unwrap();

        let mut overlay = OverlayStorage::new(base.clone_box());
        overlay.clear().unwrap();

        assert_eq!(overlay.get(&"key1".into()).unwrap(), None);
        assert_eq!(overlay.keys().unwrap().len(), 0);

        // Base should still have the value
        assert_eq!(base.get(&"key1".into()).unwrap(), Some("base_value".into()));
    }

    #[test]
    fn test_overlay_clone_box_creates_independent_copy() {
        let base: Box<dyn Storage<String, String>> =
            Box::new(StorageHashMap::<String, String>::new());

        let mut overlay = OverlayStorage::new(base);
        overlay.store("key1".into(), "value1".into()).unwrap();

        let mut cloned = overlay.clone_box();
        cloned.store("key2".into(), "value2".into()).unwrap();

        // Original should not have key2
        assert_eq!(overlay.get(&"key2".into()).unwrap(), None);
        // Clone should have both
        assert_eq!(cloned.get(&"key1".into()).unwrap(), Some("value1".into()));
        assert_eq!(cloned.get(&"key2".into()).unwrap(), Some("value2".into()));
    }

    #[test]
    fn test_overlay_count_accounts_for_tombstones() {
        let mut base: Box<dyn Storage<String, String>> =
            Box::new(StorageHashMap::<String, String>::new());
        base.store("key1".into(), "value1".into()).unwrap();
        base.store("key2".into(), "value2".into()).unwrap();

        let mut overlay = OverlayStorage::new(base);
        overlay.take(&"key1".into()).unwrap(); // Delete key1
        overlay.store("key3".into(), "value3".into()).unwrap(); // Add key3

        // Should have key2 (from base) and key3 (from overlay), but not key1 (deleted)
        assert_eq!(overlay.count().unwrap(), 2);
    }

    #[test]
    fn test_overlay_into_iter_merges_correctly() {
        let mut base: Box<dyn Storage<String, String>> =
            Box::new(StorageHashMap::<String, String>::new());
        base.store("base_key".into(), "base_value".into()).unwrap();
        base.store("deleted_key".into(), "deleted_value".into())
            .unwrap();

        let mut overlay = OverlayStorage::new(base);
        overlay
            .store("overlay_key".into(), "overlay_value".into())
            .unwrap();
        overlay.take(&"deleted_key".into()).unwrap();

        let entries: Vec<(String, String)> = overlay.into_iter().unwrap().collect();

        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&("base_key".into(), "base_value".into())));
        assert!(entries.contains(&("overlay_key".into(), "overlay_value".into())));
        assert!(
            !entries
                .iter()
                .any(|(k, _)| k == &String::from("deleted_key"))
        );
    }
}
