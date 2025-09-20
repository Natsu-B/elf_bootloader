use core::cell::UnsafeCell;
use core::ptr::read_volatile;
use core::ptr::write_volatile;

use crate::RawReg;
use crate::read_write::ReadOnly;
use crate::read_write::ReadPure;
use crate::read_write::ReadWrite;
use crate::read_write::Readable;
use crate::read_write::Writable;
use crate::read_write::WriteOnly;

#[derive(Debug)]
#[repr(transparent)]
pub struct Le<U>(UnsafeCell<U>);

#[derive(Debug)]
#[repr(transparent)]
pub struct Be<U>(UnsafeCell<U>);

impl<T: Copy + RawReg> Le<T> {
    #[inline]
    pub fn read(&self) -> T {
        unsafe { read_volatile(self.0.get()) }.from_le()
    }

    #[inline]
    pub fn write(&self, val: T) {
        unsafe { write_volatile(self.0.get(), val.to_le()) };
    }
}

impl<T: Copy + RawReg> Be<T> {
    #[inline]
    pub fn read(&self) -> T {
        unsafe { read_volatile(self.0.get()) }.from_be()
    }

    #[inline]
    pub fn write(&self, val: T) {
        unsafe { write_volatile(self.0.get(), val.to_be()) };
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
        unsafe { read_volatile((*self.0.get()).0.get()) }.from_le()
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
        unsafe { read_volatile((*self.0.get()).0.get()) }.from_le()
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
        unsafe { read_volatile((*self.0.get()).0.get()) }.from_le()
    }
}

impl<T: RawReg> Writable for WriteOnly<Le<T>> {
    type T = T;

    fn as_mut_ptr(&self) -> *mut Self::T {
        todo!()
    }

    #[inline]
    fn write(&self, val: Self::T) {
        unsafe { write_volatile((*self.0.get()).0.get(), val.to_le()) };
    }
}

impl<T: RawReg> Writable for ReadWrite<Le<T>> {
    type T = T;

    fn as_mut_ptr(&self) -> *mut Self::T {
        todo!()
    }

    #[inline]
    fn write(&self, val: Self::T) {
        unsafe { write_volatile((*self.0.get()).0.get(), val.to_le()) };
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
        unsafe { read_volatile((*self.0.get()).0.get()) }.from_be()
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
        unsafe { read_volatile((*self.0.get()).0.get()) }.from_be()
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
        unsafe { read_volatile((*self.0.get()).0.get()) }.from_be()
    }
}

impl<T: RawReg> Writable for WriteOnly<Be<T>> {
    type T = T;

    fn as_mut_ptr(&self) -> *mut Self::T {
        todo!()
    }

    #[inline]
    fn write(&self, val: Self::T) {
        unsafe { write_volatile((*self.0.get()).0.get(), val.to_be()) };
    }
}

impl<T: RawReg> Writable for ReadWrite<Be<T>> {
    type T = T;

    fn as_mut_ptr(&self) -> *mut Self::T {
        todo!()
    }

    #[inline]
    fn write(&self, val: Self::T) {
        unsafe { write_volatile((*self.0.get()).0.get(), val.to_be()) };
    }
}

impl<T: RawReg> Le<T> {
    pub fn new(t: T) -> Self {
        Self(UnsafeCell::new(t.from_le()))
    }
}

impl<T: RawReg> Be<T> {
    pub fn new(t: T) -> Self {
        Self(UnsafeCell::new(t.from_be()))
    }
}
