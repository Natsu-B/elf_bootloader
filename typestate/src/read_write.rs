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
    /// The value type read from the register.
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
    /// The value type written to the register.
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

/// Implements direct pointer access for the capability wrappers.
macro_rules! impl_access {
    (read $($wrapper:ident),+ $(,)?) => {$(
        impl<T: Copy + RawReg> Readable for $wrapper<T> {
            type T = T;
            #[inline]
            fn as_ptr(&self) -> *const T { self.0.get() }
        }
    )+};
    (write $($wrapper:ident),+ $(,)?) => {$(
        impl<T: RawReg> Writable for $wrapper<T> {
            type T = T;
            #[inline]
            fn as_mut_ptr(&self) -> *mut T { self.0.get() }
        }
    )+};
}

impl_access!(read ReadOnly, ReadPure, ReadWrite);
impl_access!(write WriteOnly, ReadWrite);

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

impl<S> ReadWrite<S>
where
    Self: Readable + Writable<T = <Self as Readable>::T>,
    <Self as Readable>::T: Copy
        + core::ops::BitAnd<Output = <Self as Readable>::T>
        + core::ops::BitOr<Output = <Self as Readable>::T>
        + core::ops::Not<Output = <Self as Readable>::T>,
{
    /// Updates the bits specified by `mask` to match `value` (read-modify-write).
    ///
    /// Equivalent to: `reg = (reg & !mask) | (value & mask)`.
    /// Bits outside `mask` are preserved; bits outside `mask` in `value` are ignored.
    /// Not suitable for clear-on-read registers.
    #[inline]
    pub fn update_bits(&self, mask: <Self as Readable>::T, value: <Self as Readable>::T) {
        let current = self.read();
        self.write((current & !mask) | (value & mask));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endianness::Le;

    #[test]
    fn update_bits_preserves_unmasked_bits() {
        let reg = ReadWrite(core::cell::UnsafeCell::new(0b1011u32));

        reg.update_bits(0b0110, 0b0100);

        assert_eq!(reg.read(), 0b1101);
    }

    #[test]
    fn update_bits_ignores_unmasked_value_bits() {
        let reg = ReadWrite(core::cell::UnsafeCell::new(0xFFFF_F0F0u32));

        reg.update_bits(0x00F0, 0xABCD);

        assert_eq!(reg.read(), 0xFFFF_F0C0);
    }

    #[test]
    fn update_bits_supports_endianness_wrappers() {
        let reg = ReadWrite(core::cell::UnsafeCell::new(Le::new(0x1234_5678u32)));

        reg.update_bits(0x00FF_0000, 0xABCD_0000);

        assert_eq!(reg.read(), 0x12CD_5678);
    }
}
