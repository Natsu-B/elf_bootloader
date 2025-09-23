use core::ptr::read_volatile;
use core::ptr::write_volatile;

use crate::RawReg;
use crate::read_write::ReadOnly;
use crate::read_write::ReadPure;
use crate::read_write::ReadWrite;
use crate::read_write::Readable;
use crate::read_write::Writable;
use crate::read_write::WriteOnly;

/// Little-endian register wrapper.
///
/// - `read()` converts the device-stored little-endian value into host
///   endianness and returns it.
/// - `write()` converts the given host-endian value to little-endian before
///   delegating the volatile write to the access wrapper.
///
/// Combine with [`ReadOnly`]/[`ReadPure`]/[`ReadWrite`] to express readable /
/// writable capabilities at the type level. Combine with [`Unaligned<T>`] to
/// safely access unaligned MMIO locations using byte-wise I/O.
///
/// Safety
/// - This type does not validate address correctness, width, or ordering. Use
///   it only with valid MMIO addresses and observe the device's access rules.
/// - Concurrent access may require external synchronization appropriate for the
///   device.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct Le<U: Copy + Clone>(pub(crate) U);

/// Big-endian register wrapper.
///
/// - `read()` converts the device-stored big-endian value into host endianness
///   and returns it.
/// - `write()` converts the given host-endian value to big-endian before
///   delegating the volatile write to the access wrapper.
///
/// Notes and safety considerations are the same as for [`Le<T>`].
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct Be<U: Copy + Clone>(pub(crate) U);

impl<T: Copy + RawReg> Le<T> {
    /// Reads a little-endian value and returns it in host endianness.
    #[inline]
    pub fn read(&self) -> T {
        self.0.from_le()
    }

    /// Writes a host-endian value after converting it to little-endian.
    #[inline]
    pub fn write(&mut self, val: T) {
        self.0 = val.to_le();
    }
}

impl<T: Copy + RawReg> Be<T> {
    /// Reads a big-endian value and returns it in host endianness.
    #[inline]
    pub fn read(&self) -> T {
        self.0.from_be()
    }

    /// Writes a host-endian value after converting it to big-endian.
    #[inline]
    pub fn write(&mut self, val: T) {
        self.0 = val.to_be();
    }
}

impl<T: Copy + RawReg> Readable for ReadOnly<Le<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        unreachable!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&(*self.0.get()).0) }.from_le()
    }
}

impl<T: Copy + RawReg> Readable for ReadPure<Le<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        unreachable!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&(*self.0.get()).0) }.from_le()
    }
}

impl<T: Copy + RawReg> Readable for ReadWrite<Le<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        unreachable!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&(*self.0.get()).0) }.from_le()
    }
}

impl<T: RawReg> Writable for WriteOnly<Le<T>> {
    type T = T;

    fn as_mut_ptr(&self) -> *mut Self::T {
        unreachable!()
    }

    #[inline]
    fn write(&self, val: Self::T) {
        unsafe { write_volatile(&mut (*self.0.get()).0, val.to_le()) };
    }
}

impl<T: RawReg> Writable for ReadWrite<Le<T>> {
    type T = T;

    fn as_mut_ptr(&self) -> *mut Self::T {
        unreachable!()
    }

    #[inline]
    fn write(&self, val: Self::T) {
        unsafe { write_volatile(&mut (*self.0.get()).0, val.to_le()) };
    }
}

impl<T: Copy + RawReg> Readable for ReadOnly<Be<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        unreachable!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&(*self.0.get()).0) }.from_be()
    }
}

impl<T: Copy + RawReg> Readable for ReadPure<Be<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        unreachable!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&(*self.0.get()).0) }.from_be()
    }
}

impl<T: Copy + RawReg> Readable for ReadWrite<Be<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        unreachable!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&(*self.0.get()).0) }.from_be()
    }
}

impl<T: RawReg> Writable for WriteOnly<Be<T>> {
    type T = T;

    fn as_mut_ptr(&self) -> *mut Self::T {
        unreachable!()
    }

    #[inline]
    fn write(&self, val: Self::T) {
        unsafe { write_volatile(&mut (*self.0.get()).0, val.to_be()) };
    }
}

impl<T: RawReg> Writable for ReadWrite<Be<T>> {
    type T = T;

    fn as_mut_ptr(&self) -> *mut Self::T {
        unreachable!()
    }

    #[inline]
    fn write(&self, val: Self::T) {
        unsafe { write_volatile(&mut (*self.0.get()).0, val.to_be()) };
    }
}

impl<T: RawReg> Le<T> {
    pub fn new(t: T) -> Self {
        Self(t.from_le())
    }
}

impl<T: RawReg> Be<T> {
    pub fn new(t: T) -> Self {
        Self(t.from_be())
    }
}
