use core::ptr::read_volatile;
use core::ptr::write_volatile;

use crate::RawReg;
use crate::read_write::ReadOnly;
use crate::read_write::ReadPure;
use crate::read_write::ReadWrite;
use crate::read_write::Readable;
use crate::read_write::Writable;
use crate::read_write::WriteOnly;

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Le<U>(U);

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Be<U>(U);

impl<T> core::ops::BitOr for Le<T>
where
    T: core::ops::BitOr<Output = T> + Copy,
{
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl<T> core::ops::BitAnd for Le<T>
where
    T: core::ops::BitAnd<Output = T> + Copy,
{
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl<T> core::ops::Not for Le<T>
where
    T: core::ops::Not<Output = T> + Copy,
{
    type Output = Self;

    fn not(self) -> Self::Output {
        Self(!self.0)
    }
}

impl<T> core::ops::BitXor for Le<T>
where
    T: core::ops::BitXor<Output = T> + Copy,
{
    type Output = Self;

    fn bitxor(self, rhs: Self) -> Self::Output {
        Self(self.0 ^ rhs.0)
    }
}

impl<T: Copy + RawReg> Le<T> {
    #[inline]
    pub fn read(&self) -> T {
        unsafe { read_volatile(&self.0) }.from_le()
    }

    #[inline]
    pub fn write(&mut self, val: T) {
        unsafe { write_volatile(&mut self.0, val.to_le()) };
    }
}

impl<T: Copy + RawReg> Be<T> {
    #[inline]
    pub fn read(&self) -> T {
        unsafe { read_volatile(&self.0) }.from_be()
    }

    #[inline]
    pub fn write(&mut self, val: T) {
        unsafe { write_volatile(&mut self.0, val.to_be()) };
    }
}

impl<T: Copy + RawReg> Readable for ReadOnly<Le<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        todo!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&self.0.0) }.from_le()
    }
}

impl<T: Copy + RawReg> Readable for ReadPure<Le<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        todo!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&self.0.0) }.from_le()
    }
}

impl<T: Copy + RawReg> Readable for ReadWrite<Le<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        todo!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&self.0.0) }.from_le()
    }
}

impl<T: RawReg> Writable for WriteOnly<Le<T>> {
    type T = T;

    fn as_mut_ptr(&mut self) -> *mut Self::T {
        todo!()
    }

    #[inline]
    fn write(&mut self, val: Self::T) {
        unsafe { write_volatile(&mut self.0.0, val.to_le()) };
    }
}

impl<T: RawReg> Writable for ReadWrite<Le<T>> {
    type T = T;

    fn as_mut_ptr(&mut self) -> *mut Self::T {
        todo!()
    }

    #[inline]
    fn write(&mut self, val: Self::T) {
        unsafe { write_volatile(&mut self.0.0, val.to_le()) };
    }
}

impl<T> core::ops::BitOr for Be<T>
where
    T: core::ops::BitOr<Output = T> + Copy,
{
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl<T> core::ops::BitAnd for Be<T>
where
    T: core::ops::BitAnd<Output = T> + Copy,
{
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl<T> core::ops::Not for Be<T>
where
    T: core::ops::Not<Output = T> + Copy,
{
    type Output = Self;

    fn not(self) -> Self::Output {
        Self(!self.0)
    }
}

impl<T> core::ops::BitXor for Be<T>
where
    T: core::ops::BitXor<Output = T> + Copy,
{
    type Output = Self;

    fn bitxor(self, rhs: Self) -> Self::Output {
        Self(self.0 ^ rhs.0)
    }
}

impl<T: Copy + RawReg> Readable for ReadOnly<Be<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        todo!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&self.0.0) }.from_be()
    }
}

impl<T: Copy + RawReg> Readable for ReadPure<Be<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        todo!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&self.0.0) }.from_be()
    }
}

impl<T: Copy + RawReg> Readable for ReadWrite<Be<T>> {
    type T = T;

    #[inline]
    fn as_ptr(&self) -> *const Self::T {
        todo!()
    }

    #[inline]
    fn read(&self) -> Self::T {
        unsafe { read_volatile(&self.0.0) }.from_be()
    }
}

impl<T: RawReg> Writable for WriteOnly<Be<T>> {
    type T = T;

    fn as_mut_ptr(&mut self) -> *mut Self::T {
        todo!()
    }

    #[inline]
    fn write(&mut self, val: Self::T) {
        unsafe { write_volatile(&mut self.0.0, val.to_be()) };
    }
}

impl<T: RawReg> Writable for ReadWrite<Be<T>> {
    type T = T;

    fn as_mut_ptr(&mut self) -> *mut Self::T {
        todo!()
    }

    #[inline]
    fn write(&mut self, val: Self::T) {
        unsafe { write_volatile(&mut self.0.0, val.to_be()) };
    }
}
