use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Per-agent state that tools keep between calls (the file-state cache, the
/// todo list, …), one value per type, created on first use. Tools stay
/// stateless and shareable; their state lives with the agent it belongs to.
#[derive(Default)]
pub struct Extensions {
    map: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.map.lock().unwrap_or_else(PoisonError::into_inner).len();
        f.debug_struct("Extensions").field("entries", &len).finish()
    }
}

impl Extensions {
    pub fn get_or_default<T: Default + Send + Sync + 'static>(&self) -> Arc<T> {
        self.get_or_insert_with(T::default)
    }

    pub fn get_or_insert_with<T: Send + Sync + 'static>(&self, make: impl FnOnce() -> T) -> Arc<T> {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = map.entry(TypeId::of::<T>()).or_insert_with(|| Arc::new(make()));
        Arc::clone(entry).downcast::<T>().unwrap_or_else(|_| unreachable!("entries are keyed by their own type"))
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        let map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(&TypeId::of::<T>()).and_then(|v| Arc::clone(v).downcast::<T>().ok())
    }

    pub fn insert<T: Send + Sync + 'static>(&self, value: T) {
        self.map.lock().unwrap_or_else(PoisonError::into_inner).insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Takes in every value of `other`, shared with it (not copied): what a
    /// host prepared once for all agents of a session.
    pub fn extend_from(&self, other: &Self) {
        let shared: Vec<_> = {
            let other = other.map.lock().unwrap_or_else(PoisonError::into_inner);
            other.iter().map(|(id, value)| (*id, Arc::clone(value))).collect()
        };
        self.map.lock().unwrap_or_else(PoisonError::into_inner).extend(shared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_value_per_type() {
        let ext = Extensions::default();
        let a = ext.get_or_default::<Mutex<Vec<u8>>>();
        a.lock().expect("lock").push(1);
        assert_eq!(*ext.get_or_default::<Mutex<Vec<u8>>>().lock().expect("lock"), vec![1]);
        assert!(ext.get::<String>().is_none());
    }

    #[test]
    fn values_taken_from_another_are_shared() {
        let seed = Extensions::default();
        seed.insert(String::from("session"));
        let ext = Extensions::default();
        ext.extend_from(&seed);
        let (a, b) = (seed.get::<String>().expect("seeded"), ext.get::<String>().expect("taken"));
        assert!(Arc::ptr_eq(&a, &b));
    }
}
