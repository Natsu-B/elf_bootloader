#![no_std]
//! MMIO typestate wrapper.
//!
//! This crate provides small wrapper types around MMIO registers that encode
//! readable / writable capabilities at the type level (typestate).
//!
//! All reads and writes are performed with volatile operations to prevent the
//! compiler from eliding or reordering access to memory-mapped registers.
//!
//! # Typestates
//! - [`ReadOnly<T>`]: readable, no write API is exposed. Reads **may** have side effects.
//! - [`ReadPure<T>`]: readable **without side effects** (safe to poll). Still uses volatile reads.
//! - [`WriteOnly<T>`]: writable, no read API is exposed.
//! - [`ReadWrite<T>`]: both readable and writable.
//!
//! # Safety
//! These wrappers do not validate that the underlying address actually maps to
//! device registers. It is **your** responsibility to place these wrappers at
//! the correct, valid MMIO address and to follow the device's access rules.

use core::ptr::read_volatile;
use core::ptr::write_volatile;

/// Readable register (no write API exposed).
///
/// Reads are performed with `read_volatile`. Depending on the hardware,
/// reading may have side effects (e.g., clear-on-read fields).
#[derive(Clone, Debug)]
#[repr(transparent)]
pub struct ReadOnly<T>(T);

/// Readable register **without side effects** (safe to poll).
///
/// Access still uses `read_volatile` to prevent elision/reordering, but this
/// type expresses the contract that repeated reads do not change device state.
#[derive(Clone, Debug)]
#[repr(transparent)]
pub struct ReadPure<T>(T);

/// Write-only register (no read API exposed).
#[derive(Debug)]
#[repr(transparent)]
pub struct WriteOnly<T>(T);

/// Read/write register.
#[derive(Debug)]
#[repr(transparent)]
pub struct ReadWrite<T>(T);

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
    fn as_mut_ptr(&mut self) -> *mut Self::T;

    /// Performs a single volatile write to the underlying address.
    #[inline]
    fn write(&mut self, val: Self::T) {
        unsafe { write_volatile(self.as_mut_ptr(), val) }
    }
}

impl<T: Copy> Readable for ReadOnly<T> {
    type T = T;
    #[inline]
    fn as_ptr(&self) -> *const T {
        &self.0
    }
}

impl<T: Copy> Readable for ReadPure<T> {
    type T = T;
    #[inline]
    fn as_ptr(&self) -> *const T {
        &self.0
    }
}

impl<T: Copy> Readable for ReadWrite<T> {
    type T = T;
    #[inline]
    fn as_ptr(&self) -> *const T {
        &self.0
    }
}

impl<T> Writable for WriteOnly<T> {
    type T = T;
    #[inline]
    fn as_mut_ptr(&mut self) -> *mut T {
        &mut self.0
    }
}

impl<T> Writable for ReadWrite<T> {
    type T = T;
    #[inline]
    fn as_mut_ptr(&mut self) -> *mut T {
        &mut self.0
    }
}
