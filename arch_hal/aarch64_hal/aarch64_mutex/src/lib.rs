//! IRQ-save lock wrappers built on raw mutex primitives.

#![no_std]
#![cfg_attr(all(test, target_arch = "aarch64"), no_main)]
#![cfg_attr(all(test, target_arch = "aarch64"), feature(custom_test_frameworks))]
#![cfg_attr(
    all(test, target_arch = "aarch64"),
    test_runner(aarch64_unit_test::test_runner)
)]
#![cfg_attr(
    all(test, target_arch = "aarch64"),
    reexport_test_harness_main = "test_main"
)]

use core::ops::Deref;
use core::ops::DerefMut;

pub struct RawSpinLockIrqSave<T> {
    inner: mutex::RawSpinLock<T>,
}

impl<T> RawSpinLockIrqSave<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: mutex::RawSpinLock::new(value),
        }
    }

    pub fn lock_irqsave(&self) -> RawSpinLockIrqSaveGuard<'_, T> {
        let flags = cpu::irq_save();
        let guard = self.inner.lock();
        RawSpinLockIrqSaveGuard {
            flags,
            guard: Some(guard),
        }
    }
}

pub struct RawSpinLockIrqSaveGuard<'a, T> {
    flags: u64,
    guard: Option<mutex::RawSpinLockGuard<'a, T>>,
}

impl<T> Deref for RawSpinLockIrqSaveGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard
            .as_ref()
            .expect("spinlock guard must be present while guard is alive")
    }
}

impl<T> DerefMut for RawSpinLockIrqSaveGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard
            .as_mut()
            .expect("spinlock guard must be present while guard is alive")
    }
}

impl<T> Drop for RawSpinLockIrqSaveGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(guard);
        }
        cpu::irq_restore(self.flags);
    }
}

pub struct RawRwLockIrqSave<T> {
    inner: mutex::RawRwLock<T>,
}

impl<T> RawRwLockIrqSave<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: mutex::RawRwLock::new(value),
        }
    }

    pub fn read_irqsave(&self) -> RawRwLockReadIrqSaveGuard<'_, T> {
        let flags = cpu::irq_save();
        let guard = self.inner.read();
        RawRwLockReadIrqSaveGuard {
            flags,
            guard: Some(guard),
        }
    }

    pub fn write_irqsave(&self) -> RawRwLockWriteIrqSaveGuard<'_, T> {
        let flags = cpu::irq_save();
        let guard = self.inner.write();
        RawRwLockWriteIrqSaveGuard {
            flags,
            guard: Some(guard),
        }
    }
}

pub struct RawRwLockReadIrqSaveGuard<'a, T> {
    flags: u64,
    guard: Option<mutex::RawRwLockReadGuard<'a, T>>,
}

impl<T> Deref for RawRwLockReadIrqSaveGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard
            .as_ref()
            .expect("read guard must be present while guard is alive")
    }
}

impl<T> Drop for RawRwLockReadIrqSaveGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(guard);
        }
        cpu::irq_restore(self.flags);
    }
}

pub struct RawRwLockWriteIrqSaveGuard<'a, T> {
    flags: u64,
    guard: Option<mutex::RawRwLockWriteGuard<'a, T>>,
}

impl<T> Deref for RawRwLockWriteIrqSaveGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard
            .as_ref()
            .expect("write guard must be present while guard is alive")
    }
}

impl<T> DerefMut for RawRwLockWriteIrqSaveGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard
            .as_mut()
            .expect("write guard must be present while guard is alive")
    }
}

impl<T> Drop for RawRwLockWriteIrqSaveGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(guard);
        }
        cpu::irq_restore(self.flags);
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
fn __unit_test_init() {
    aarch64_unit_test::init_default_uart();
    exceptions::setup_exception();
}

#[cfg(all(test, target_arch = "aarch64"))]
aarch64_unit_test::uboot_unit_test_harness!(__unit_test_init);

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;
    use core::arch::asm;

    fn clear_irq() {
        unsafe {
            asm!("msr daifclr, #2", options(nostack));
        }
        cpu::isb();
    }

    #[test_case]
    fn spinlock_irqsave_restores_irq_mask() {
        clear_irq();
        let initial = cpu::read_daif();
        assert_eq!(initial & 0x80, 0);

        let lock = RawSpinLockIrqSave::new(());
        {
            let _guard = lock.lock_irqsave();
            assert_ne!(cpu::read_daif() & 0x80, 0);
        }
        assert_eq!(cpu::read_daif(), initial);
    }

    #[test_case]
    fn spinlock_nested_restore_on_outer_drop() {
        clear_irq();
        let initial = cpu::read_daif();
        let lock = RawSpinLockIrqSave::new(());
        let outer = lock.lock_irqsave();
        {
            let _inner = lock.lock_irqsave();
            assert_ne!(cpu::read_daif() & 0x80, 0);
        }
        assert_ne!(cpu::read_daif() & 0x80, 0);
        drop(outer);
        assert_eq!(cpu::read_daif(), initial);
    }

    #[test_case]
    fn rwlock_read_irqsave_restores() {
        clear_irq();
        let initial = cpu::read_daif();
        let lock = RawRwLockIrqSave::new(());
        {
            let _guard = lock.read_irqsave();
            assert_ne!(cpu::read_daif() & 0x80, 0);
        }
        assert_eq!(cpu::read_daif(), initial);
    }

    #[test_case]
    fn rwlock_write_irqsave_nested_restore() {
        clear_irq();
        let initial = cpu::read_daif();
        let lock = RawRwLockIrqSave::new(());
        let write_guard = lock.write_irqsave();
        {
            let _read_guard = lock.read_irqsave();
            assert_ne!(cpu::read_daif() & 0x80, 0);
        }
        assert_ne!(cpu::read_daif() & 0x80, 0);
        drop(write_guard);
        assert_eq!(cpu::read_daif(), initial);
    }
}
