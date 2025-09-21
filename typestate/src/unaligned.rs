use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::addr_of;
use core::ptr::{self};

use crate::Be;
use crate::Le;
use crate::RawReg;

/// Unaligned register wrapper that performs byte-wise volatile I/O.
///
/// Many MMIO blocks require aligned accesses of a specific width. When a
/// register is only byte-addressable or the target address is not naturally
/// aligned for `T`, this wrapper reads/writes one byte at a time with
/// `read_volatile`/`write_volatile` to avoid unaligned loads/stores.
///
/// This type intentionally does not perform any endianness conversion. Combine
/// it with [`Le<Unaligned<T>>`] or [`Be<Unaligned<T>>`] when the register value
/// is stored in little-/big-endian byte order on the device.
///
/// Safety
/// - This type avoids Rust-level UB from unaligned access by only touching the
///   device via `u8` volatile operations and assembling the value in a properly
///   aligned temporary.
/// - It does not guarantee that byte-wise access is correct for your device.
///   Some devices require full-width atomic accesses; consult the hardware
///   manual before using this wrapper.
/// - `T` must be a trivially copyable integer-like type. In practice this means
///   using one of the provided `RawReg` implementations (e.g., `u8..u128`),
///   where all bit patterns are valid.

#[repr(packed)]
pub struct Unaligned<T>(UnsafeCell<T>);

impl<T: Copy + RawReg> Unaligned<T> {
    /// Reads `size_of::<T>()` bytes via `read_volatile` and returns the value.
    #[inline]
    fn _read(cell: *const UnsafeCell<T>) -> T {
        let data: *const u8 = UnsafeCell::raw_get(cell) as *const u8;

        let mut tmp = MaybeUninit::<T>::uninit();
        let dst = tmp.as_mut_ptr() as *mut u8;

        for i in 0..core::mem::size_of::<T>() {
            unsafe { ptr::write(dst.add(i), ptr::read_volatile(data.add(i))) };
        }

        unsafe { tmp.assume_init() }
    }

    /// Writes `size_of::<T>()` bytes via `write_volatile`.
    #[inline]
    fn _write(cell: *const UnsafeCell<T>, val: T) {
        let data: *mut u8 = UnsafeCell::raw_get(cell) as *mut u8;

        let src = &val as *const T as *const u8;
        for i in 0..core::mem::size_of::<T>() {
            unsafe { ptr::write_volatile(data.add(i), ptr::read(src.add(i))) };
        }
    }

    /// Reads from an unaligned location without invoking unaligned loads.
    #[inline]
    pub fn read(&self) -> T {
        Unaligned::_read(addr_of!(self.0))
    }

    /// Writes to an unaligned location without invoking unaligned stores.
    #[inline]
    pub fn write(&self, val: T) {
        Unaligned::_write(addr_of!(self.0), val);
    }
}

impl<T: Copy + RawReg> Le<Unaligned<T>> {
    /// Reads a little-endian value from an unaligned location.
    #[inline]
    pub fn read(&self) -> T {
        let le_cell = addr_of!(self.0);
        let ua_mut = UnsafeCell::raw_get(le_cell);
        let data_cell = unsafe { addr_of!((*ua_mut).0) };
        Unaligned::_read(data_cell).from_le()
    }

    /// Writes a little-endian value to an unaligned location.
    #[inline]
    pub fn write(&self, val: T) {
        let le_cell = addr_of!(self.0);
        let ua_mut = UnsafeCell::raw_get(le_cell);
        let data_cell = unsafe { addr_of!((*ua_mut).0) };
        Unaligned::_write(data_cell, val.to_le());
    }
}

impl<T: Copy + RawReg> Be<Unaligned<T>> {
    /// Reads a big-endian value from an unaligned location.
    #[inline]
    pub fn read(&self) -> T {
        let be_cell = addr_of!(self.0);
        let ua_mut = UnsafeCell::raw_get(be_cell);
        let data_cell = unsafe { addr_of!((*ua_mut).0) };
        Unaligned::_read(data_cell).from_be()
    }

    /// Writes a big-endian value to an unaligned location.
    #[inline]
    pub fn write(&self, val: T) {
        let be_cell = addr_of!(self.0);
        let ua_mut = UnsafeCell::raw_get(be_cell);
        let data_cell = unsafe { addr_of!((*ua_mut).0) };
        Unaligned::_write(data_cell, val.to_be());
    }
}
