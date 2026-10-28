use std::hash::Hash;

use serde::{Deserialize, Serialize};
use surfpool_types::FifoMap;

impl<K, V> super::Storage<K, V> for FifoMap<K, V>
where
    K: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static + std::cmp::Eq + Hash,
    V: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn store(&mut self, key: K, value: V) -> super::StorageResult<()> {
        self.insert(key, value);
        Ok(())
    }

    fn clear(&mut self) -> super::StorageResult<()> {
        self.clear();
        Ok(())
    }

    fn get(&self, key: &K) -> super::StorageResult<Option<V>> {
        Ok(self.get(key).cloned())
    }

    fn take(&mut self, key: &K) -> super::StorageResult<Option<V>> {
        Ok(self.remove(key))
    }

    fn keys(&self) -> super::StorageResult<Vec<K>> {
        Ok(self.iter().map(|(k, _)| k.clone()).collect())
    }

    fn into_iter(&self) -> super::StorageResult<Box<dyn Iterator<Item = (K, V)> + '_>> {
        Ok(Box::new(self.iter().map(|(k, v)| (k.clone(), v.clone()))))
    }

    fn clone_box(&self) -> Box<dyn super::Storage<K, V>> {
        Box::new(self.clone())
    }

    fn contains_key(&self, key: &K) -> super::StorageResult<bool> {
        Ok(self.contains_key(key))
    }

    fn count(&self) -> super::StorageResult<u64> {
        Ok(self.len() as u64)
    }
}

/// The in-memory profile store. Unlike `FifoMap::clone`, which keeps only the entries, a clone
/// keeps the capacity, so a copy made on write evicts at the same size.
pub(crate) struct ProfileFifoMap<K, V>(pub(crate) FifoMap<K, V>);

impl<K: Hash + Eq + Clone, V: Clone> Clone for ProfileFifoMap<K, V> {
    fn clone(&self) -> Self {
        let mut map = FifoMap::new(self.0.capacity());
        for (key, value) in self.0.iter() {
            map.insert(key.clone(), value.clone());
        }
        Self(map)
    }
}

impl<K, V> super::Storage<K, V> for ProfileFifoMap<K, V>
where
    K: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static + std::cmp::Eq + Hash,
    V: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn store(&mut self, key: K, value: V) -> super::StorageResult<()> {
        self.0.store(key, value)
    }

    fn clear(&mut self) -> super::StorageResult<()> {
        super::Storage::clear(&mut self.0)
    }

    fn get(&self, key: &K) -> super::StorageResult<Option<V>> {
        super::Storage::get(&self.0, key)
    }

    fn take(&mut self, key: &K) -> super::StorageResult<Option<V>> {
        self.0.take(key)
    }

    fn keys(&self) -> super::StorageResult<Vec<K>> {
        self.0.keys()
    }

    fn into_iter(&self) -> super::StorageResult<Box<dyn Iterator<Item = (K, V)> + '_>> {
        super::Storage::into_iter(&self.0)
    }

    fn clone_box(&self) -> Box<dyn super::Storage<K, V>> {
        Box::new(self.clone())
    }

    fn contains_key(&self, key: &K) -> super::StorageResult<bool> {
        super::Storage::contains_key(&self.0, key)
    }

    fn count(&self) -> super::StorageResult<u64> {
        self.0.count()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::storage::Storage;

    #[test]
    fn copy_on_write_keeps_the_fifo_capacity() {
        let mut live: Arc<ProfileFifoMap<u32, u32>> = Arc::new(ProfileFifoMap(FifoMap::new(2)));
        live.store(0, 0).unwrap();
        // A sandbox holds the map, so the next write copies it.
        let sandbox = live.clone_box();
        for i in 1..=3 {
            live.store(i, i).unwrap();
        }
        assert_eq!(live.keys().unwrap(), vec![2, 3]);
        assert_eq!(sandbox.keys().unwrap(), vec![0]);
    }
}
