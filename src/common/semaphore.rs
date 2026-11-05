// A number of slots, like the builds or the compilers running at the same time
use std::sync::{Condvar, Mutex};

pub struct Semaphore {
    free: Mutex<usize>,
    released: Condvar,
}

/// One of the slots, free again when it is dropped
pub struct Permit<'a>(&'a Semaphore);

impl Semaphore {
    /// At least one
    pub fn new(count: usize) -> Self {
        return Semaphore {
            free: Mutex::new(count.max(1)),
            released: Condvar::new(),
        };
    }

    /// Waits until one is free
    pub fn acquire(&self) -> Permit<'_> {
        let mut free = self.free.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        while *free == 0 {
            free = self.released.wait(free).unwrap_or_else(|poisoned| poisoned.into_inner());
        }

        *free -= 1;
        return Permit(self);
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
        self.0.released.notify_one();
    }
}
