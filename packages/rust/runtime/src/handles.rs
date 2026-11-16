use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Live objects of one handle type. A handle's value is its id; 0 is never issued, so
/// `XR_NULL_HANDLE` is always invalid.
pub struct Registry<T> {
    next: AtomicU64,
    items: RwLock<HashMap<u64, Arc<T>>>,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Self {
            next: AtomicU64::new(1),
            items: RwLock::default(),
        }
    }
}

impl<T> Registry<T> {
    pub fn insert(&self, item: T) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.items
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::new(item));
        id
    }

    pub fn get(&self, id: u64) -> Option<Arc<T>> {
        self.items
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    pub fn remove(&self, id: u64) -> Option<Arc<T>> {
        self.items
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
    }

    pub fn all(&self) -> Vec<(u64, Arc<T>)> {
        self.items
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(&id, item)| (id, item.clone()))
            .collect()
    }

    /// Removes every object `doomed` selects; used to destroy children with their parent.
    pub fn remove_where(&self, mut doomed: impl FnMut(&T) -> bool) {
        self.items
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, item| !doomed(item));
    }
}

pub fn handle<H>(id: u64) -> *mut H {
    id as usize as *mut H
}

pub fn id<H>(handle: *mut H) -> u64 {
    handle as usize as u64
}
