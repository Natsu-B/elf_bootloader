use core::cell::UnsafeCell;
use core::ptr::read_volatile;
use core::ptr::write_volatile;

use crate::RawReg;

/// Readable register (no write API exposed).
///
/// Reads are performed with `read_volatile`. Depending on the hardware,
/// reading may have side effects (e.g., clear-on-read fields).
#[derive(Debug)]
#[repr(transparent)]
pub struct ReadOnly<T>(pub(crate) UnsafeCell<T>);

/// Readable register **without side effects** (safe to poll).
///
/// Access still uses `read_volatile` to prevent elision/reordering, but this
/// type expresses the contract that repeated reads do not change device state.
#[derive(Debug)]
#[repr(transparent)]
pub struct ReadPure<T>(pub(crate) UnsafeCell<T>);

/// Write-only register (no read API exposed).
#[derive(Debug)]
#[repr(transparent)]
pub struct WriteOnly<T>(pub(crate) UnsafeCell<T>);

/// Read/write register.
#[derive(Debug)]
#[repr(transparent)]
pub struct ReadWrite<T>(pub(crate) UnsafeCell<T>);

/// Volatile-readable capability.
///
/// `T` must be `Copy` so the read value can be returned by value.
/// Consider constraining `T` further (e.g. a `Pod`-like bound) if you need
/// "all bit patterns are valid".
pub trait Readable {
    type T: Copy;

    /// Returns a pointer to the underlying storage.
    ///
    /// # Safety
    /// The caller must ensure this points at a valid MMIO location for `T`.
    fn as_ptr(&self) -> *const Self::T;

    /// Performs a single volatile read from the underlying address.
    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(self.as_ptr()) }
    }
}

/// Volatile-writable capability.
///
/// No `Copy` bound is required: values are moved into the register.
pub trait Writable {
    type T;

    /// Returns a mutable pointer to the underlying storage.
    ///
    /// # Safety
    /// The caller must ensure this points at a valid MMIO location for `T`.
    fn as_mut_ptr(&self) -> *mut Self::T;

    /// Performs a single volatile write to the underlying address.
    #[inline]
    fn write(&self, val: Self::T) {
        unsafe { write_volatile(self.as_mut_ptr(), val) }
    }
}

impl<T: Copy + RawReg> Readable for ReadOnly<T> {
    type T = T;
    #[inline]
    fn as_ptr(&self) -> *const T {
        self.0.get()
    }
}

impl<T: Copy + RawReg> Readable for ReadPure<T> {
    type T = T;
    #[inline]
    fn as_ptr(&self) -> *const T {
        self.0.get()
    }
}

impl<T: Copy + RawReg> Readable for ReadWrite<T> {
    type T = T;
    #[inline]
    fn as_ptr(&self) -> *const T {
        self.0.get()
    }
}

impl<T: RawReg> Writable for WriteOnly<T> {
    type T = T;
    #[inline]
    fn as_mut_ptr(&self) -> *mut T {
        self.0.get()
    }
}

impl<T: RawReg> Writable for ReadWrite<T> {
    type T = T;
    #[inline]
    fn as_mut_ptr(&self) -> *mut T {
        self.0.get()
    }
}

impl<T> ReadWrite<T>
where
    Self: Writable<T = T> + Readable<T = T>,
    T: Copy + core::ops::BitOr<Output = T>,
{
    /// Sets the bits specified by `mask` (read-modify-write).
    #[inline]
    pub fn set_bits(&self, mask: T) {
        let current = self.read();
        self.write(current | mask);
    }
}

impl<T> ReadWrite<T>
where
    Self: Writable<T = T> + Readable<T = T>,
    T: Copy + core::ops::BitAnd<Output = T> + core::ops::Not<Output = T>,
{
    /// Clears the bits specified by `mask` (read-modify-write).
    #[inline]
    pub fn clear_bits(&self, mask: T) {
        let current = self.read();
        self.write(current & !mask);
    }
}

impl<T> ReadWrite<T>
where
    Self: Writable<T = T> + Readable<T = T>,
    T: Copy + core::ops::BitXor<Output = T>,
{
    /// Toggles the bits specified by `mask` (read-modify-write).
    #[inline]
    pub fn toggle_bits(&self, mask: T) {
        let current: <ReadWrite<T> as Readable>::T = self.read();
        self.write(current ^ mask);
    }
}
