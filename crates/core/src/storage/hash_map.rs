pub use std::collections::HashMap;
use std::hash::Hash;

use serde::{Deserialize, Serialize};

impl<K, V> super::Storage<K, V> for HashMap<K, V>
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
        Ok(self.keys().cloned().collect())
    }

    fn into_iter(&self) -> super::StorageResult<Box<dyn Iterator<Item = (K, V)> + '_>> {
        Ok(Box::new(self.clone().into_iter()))
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

    fn modify(
        &mut self,
        key: K,
        f: &mut dyn FnMut(&mut Option<V>) -> bool,
    ) -> super::StorageResult<()> {
        let mut value = self.remove(&key);
        f(&mut value);
        if let Some(value) = value {
            self.insert(key, value);
        }
        Ok(())
    }
}
