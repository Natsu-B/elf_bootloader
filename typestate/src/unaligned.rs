use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::addr_of;
use core::ptr::addr_of_mut;
use core::ptr::read_unaligned;
use core::ptr::write_unaligned;
use core::ptr::{self};

use crate::Be;
use crate::Le;
use crate::RawReg;
use crate::ReadOnly;
use crate::ReadPure;
use crate::ReadWrite;
use crate::WriteOnly;

/// Unaligned register wrapper that provides unaligned-safe representation.
///
/// Many MMIO blocks require aligned accesses of a specific width. When a
/// register is only byte-addressable or the target address is not naturally
/// aligned for `T`, this wrapper avoids unaligned loads/stores by assembling
/// values in a temporary. Volatile byte-wise I/O is always performed through
/// an access-capability wrapper.
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
#[repr(transparent)]
#[derive(Debug, Copy, Clone)]
pub struct Unaligned<T>(T);

impl<T: Copy + RawReg> Unaligned<T> {
    /// Reads from an unaligned location without invoking unaligned loads.
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { read_unaligned(ptr) }.0
    }

    /// Writes to an unaligned location without invoking unaligned stores.
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe { write_unaligned(ptr, Unaligned(val)) };
    }
}

impl<T: Copy + RawReg> Le<Unaligned<T>> {
    /// Reads a little-endian value from an unaligned location.
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { read_unaligned(ptr) }.0.0.from_le()
    }

    /// Writes a little-endian value to an unaligned location.
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe {
            write_unaligned(ptr, Le(Unaligned(val.to_le())));
        }
    }
}

mod volatile {
    use super::*;

    /// Reads `size_of::<T>()` bytes via `read_volatile` and returns the value.
    #[inline]
    pub(crate) unsafe fn read<T: RawReg>(data: *const T) -> T {
        let data: *const u8 = data as *const u8;

        let mut tmp = MaybeUninit::<T>::uninit();
        let dst = tmp.as_mut_ptr() as *mut u8;

        for i in 0..core::mem::size_of::<T>() {
            unsafe { ptr::write(dst.add(i), ptr::read_volatile(data.add(i))) };
        }

        unsafe { tmp.assume_init() }
    }

    /// Writes `size_of::<T>()` bytes via `write_volatile`.
    #[inline]
    pub(crate) unsafe fn write<T: RawReg>(data: *mut T, val: T) {
        let data: *mut u8 = data as *mut u8;

        let src = &val as *const T as *const u8;
        for i in 0..core::mem::size_of::<T>() {
            unsafe { ptr::write_volatile(data.add(i), ptr::read(src.add(i))) };
        }
    }
}

impl<T: Copy + RawReg> Be<Unaligned<T>> {
    /// Reads a big-endian value from an unaligned location.
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { read_unaligned(ptr) }.0.0.from_be()
    }

    /// Writes a big-endian value to an unaligned location.
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe {
            write_unaligned(ptr, Be(Unaligned(val.to_be())));
        }
    }
}

impl<T: Copy + RawReg> ReadOnly<Le<Unaligned<T>>> {
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { volatile::read(addr_of!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0)) }
            .from_le()
    }
}

impl<T: Copy + RawReg> ReadPure<Le<Unaligned<T>>> {
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { volatile::read(addr_of!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0)) }
            .from_le()
    }
}

impl<T: Copy + RawReg> ReadWrite<Le<Unaligned<T>>> {
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { volatile::read(addr_of!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0)) }
            .from_le()
    }
}

impl<T: RawReg> WriteOnly<Le<Unaligned<T>>> {
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe {
            volatile::write(
                addr_of_mut!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0),
                val.to_le(),
            )
        };
    }
}

impl<T: RawReg> ReadWrite<Le<Unaligned<T>>> {
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe {
            volatile::write(
                addr_of_mut!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0),
                val.to_le(),
            )
        };
    }
}

impl<T: Copy + RawReg> ReadOnly<Be<Unaligned<T>>> {
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { volatile::read(addr_of!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0)) }
            .from_be()
    }
}

impl<T: Copy + RawReg> ReadPure<Be<Unaligned<T>>> {
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { volatile::read(addr_of!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0)) }
            .from_be()
    }
}

impl<T: Copy + RawReg> ReadWrite<Be<Unaligned<T>>> {
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { volatile::read(addr_of!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0)) }
            .from_be()
    }
}

impl<T: RawReg> WriteOnly<Be<Unaligned<T>>> {
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe {
            volatile::write(
                addr_of_mut!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0),
                val.to_be(),
            )
        };
    }
}

impl<T: RawReg> ReadWrite<Be<Unaligned<T>>> {
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe {
            volatile::write(
                addr_of_mut!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0.0),
                val.to_be(),
            )
        };
    }
}

impl<T: Copy + RawReg> ReadOnly<Unaligned<T>> {
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { volatile::read(addr_of!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0)) }
    }
}

impl<T: Copy + RawReg> ReadPure<Unaligned<T>> {
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { volatile::read(addr_of!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0)) }
    }
}

impl<T: Copy + RawReg> ReadWrite<Unaligned<T>> {
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { volatile::read(addr_of!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0)) }
    }
}

impl<T: RawReg> WriteOnly<Unaligned<T>> {
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe {
            volatile::write(
                addr_of_mut!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0),
                val,
            )
        };
    }
}

impl<T: RawReg> ReadWrite<Unaligned<T>> {
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe {
            volatile::write(
                addr_of_mut!((*UnsafeCell::raw_get(addr_of!((*ptr).0))).0),
                val,
            )
        };
    }
}
