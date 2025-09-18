#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
use core::fmt;
use core::ops::Deref;
use core::ops::DerefMut;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;

pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinLockGuard { lock: self }
    }

    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        if self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SpinLockGuard { lock: self })
        } else {
            None
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for SpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(_g) = self.try_lock() {
            f.debug_struct("SpinLock")
                .field("data", unsafe { &*self.data.get() })
                .finish()
        } else {
            f.debug_struct("SpinLock")
                .field("data", &"<locked>")
                .finish()
        }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.data.get() }
    }
}

pub struct RwLock<T> {
    /// The most significant bit is the write lock flag.
    /// The other bits are the read count.
    read_count_write_lock_flag: AtomicUsize,
    data: UnsafeCell<T>,
}

pub struct RwLockReadGuard<'a, T> {
    lock: &'a RwLock<T>,
}

pub struct RwLockWriteGuard<'a, T> {
    lock: &'a RwLock<T>,
}

unsafe impl<T: Send + Sync> Sync for RwLock<T> {}
unsafe impl<T: Send> Send for RwLock<T> {}

impl<T> RwLock<T> {
    const WRITE_FLAG: usize = 1 << (usize::BITS - 1);
    pub const fn new(data: T) -> Self {
        Self {
            read_count_write_lock_flag: AtomicUsize::new(0),
            data: UnsafeCell::new(data),
        }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        loop {
            let current_state = self.read_count_write_lock_flag.load(Ordering::Relaxed);
            // If no write lock is held or requested, try to acquire a read lock.
            if current_state & Self::WRITE_FLAG == 0
                && self
                    .read_count_write_lock_flag
                    .compare_exchange_weak(
                        current_state,
                        current_state + 1,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                return RwLockReadGuard { lock: self };
            }
            core::hint::spin_loop();
        }
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        loop {
            let current_state = self.read_count_write_lock_flag.load(Ordering::Relaxed);
            // If no write lock is held, try to acquire one.
            if current_state & Self::WRITE_FLAG == 0 {
                // Attempt to set the write flag.
                if self
                    .read_count_write_lock_flag
                    .compare_exchange_weak(
                        current_state,
                        current_state | Self::WRITE_FLAG,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    // Wait for all existing readers to finish.
                    while self.read_count_write_lock_flag.load(Ordering::Relaxed)
                        & !Self::WRITE_FLAG
                        != 0
                    {
                        core::hint::spin_loop();
                    }
                    return RwLockWriteGuard { lock: self };
                }
            }
            // Spin if a write lock is already held or if CAS failed.
            core::hint::spin_loop();
        }
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock
            .read_count_write_lock_flag
            .fetch_sub(1, Ordering::Release);
    }
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock
            .read_count_write_lock_flag
            .fetch_and(!RwLock::<T>::WRITE_FLAG, Ordering::Release);
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.data.get() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn spinlock_test() {
        let spinlock = Arc::new(SpinLock::new(0));
        let mut handles = vec![];

        for _ in 0..10 {
            let spinlock_clone = Arc::clone(&spinlock);
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    let mut guard = spinlock_clone.lock();
                    *guard += 1;
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(*spinlock.lock(), 10 * 1000);
    }

    #[test]
    fn rw_lock_test() {
        let lock = Arc::new(RwLock::new(0));
        let mut handles = vec![];

        // A single writer thread that increments the value.
        let writer_lock_clone = Arc::clone(&lock);
        handles.push(thread::spawn(move || {
            for _ in 0..100 {
                let mut writer = writer_lock_clone.write();
                *writer += 1;
                thread::sleep(Duration::from_millis(1));
            }
        }));

        // Multiple reader threads that read the value.
        for _ in 0..10 {
            let reader_lock_clone = Arc::clone(&lock);
            handles.push(thread::spawn(move || {
                // Read multiple times to increase the chance of observing different values.
                for _ in 0..50 {
                    let reader = reader_lock_clone.read();
                    let value = *reader;
                    // The value should be within the expected range.
                    assert!(value >= 0 && value <= 100);
                    thread::sleep(Duration::from_millis(1));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // After all threads are done, the final value should be 100.
        assert_eq!(*lock.read(), 100);
    }
}
