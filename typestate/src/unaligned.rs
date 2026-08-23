use core::mem::MaybeUninit;
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

/// Internal macro for unaligned volatile reads.
///
/// Reads a value from an unaligned memory location using byte-wise I/O.
#[macro_export]
macro_rules! unalign_read {
    ($v:expr => $ty:ty) => {
        unsafe {
            let _: $ty = $v;

            <$ty>::read(::core::ptr::addr_of!($v))
        }
    };
}

/// Internal macro for unaligned volatile writes.
///
/// Writes a value to an unaligned memory location using byte-wise I/O.
#[macro_export]
macro_rules! unalign_write {
    ($v:expr => WriteOnly<Unaligned<$t:ty>>, $val:expr) => {{
        {
            let _: &WriteOnly<Unaligned<$t>> = &$v;
        }
        unsafe { <WriteOnly<Unaligned<$t>>>::write(::core::ptr::addr_of_mut!($v), $val) };
    }};
    ($v:expr => ReadWrite<Unaligned<$t:ty>>, $val:expr) => {{
        {
            let _: &ReadWrite<Unaligned<$t>> = &$v;
        }
        unsafe { <ReadWrite<Unaligned<$t>>>::write(::core::ptr::addr_of_mut!($v), $val) };
    }};
    ($v:expr => $ty:ty, $val:expr) => {{
        {
            let _: &mut $ty = &mut $v;
        }
        unsafe { <$ty>::write(::core::ptr::addr_of_mut!($v), $val) };
    }};
}

impl<T: Copy + RawReg> Unaligned<T> {
    /// Reads from an unaligned location using unaligned-safe load.
    ///
    /// # Safety
    /// - `ptr` must point to valid, readable memory for `size_of::<T>()` bytes.
    /// - The memory need not be aligned for `T`.
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { read_unaligned(ptr) }.0
    }

    /// Writes to an unaligned location using unaligned-safe store.
    ///
    /// # Safety
    /// - `ptr` must point to valid, writable memory for `size_of::<T>()` bytes.
    /// - The memory need not be aligned for `T`.
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe { write_unaligned(ptr, Unaligned(val)) };
    }
}

impl<T: Copy + RawReg> Le<Unaligned<T>> {
    /// Reads a little-endian value from an unaligned location.
    ///
    /// # Safety
    /// - `ptr` must point to valid, readable memory for `size_of::<T>()` bytes.
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { read_unaligned(ptr) }.0.0.from_le()
    }

    /// Writes a little-endian value to an unaligned location.
    ///
    /// # Safety
    /// - `ptr` must point to valid, writable memory for `size_of::<T>()` bytes.
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
    ///
    /// # Safety
    /// - `ptr` must point to valid, readable memory for `size_of::<T>()` bytes.
    #[inline]
    pub unsafe fn read(ptr: *const Self) -> T {
        unsafe { read_unaligned(ptr) }.0.0.from_be()
    }

    /// Writes a big-endian value to an unaligned location.
    ///
    /// # Safety
    /// - `ptr` must point to valid, writable memory for `size_of::<T>()` bytes.
    #[inline]
    pub unsafe fn write(ptr: *mut Self, val: T) {
        unsafe {
            write_unaligned(ptr, Be(Unaligned(val.to_be())));
        }
    }
}

/// Implements byte-wise volatile I/O for one unaligned storage representation.
///
/// All wrapper layers are transparent; casting their raw pointer reaches `T`
/// without a reference, then volatile helpers access it one byte at a time.
macro_rules! impl_unaligned_access {
    ($t:ident, $storage:ty, $from:ident, $to:ident) => {
        impl_unaligned_access!(@read $t, $storage, $from; ReadOnly, ReadPure, ReadWrite);
        impl_unaligned_access!(@write $t, $storage, $to; WriteOnly, ReadWrite);
    };
    (@read $t:ident, $storage:ty, $from:ident; $($access:ident),+) => {
        $(
            impl<$t: Copy + RawReg> $access<$storage> {
                /// # Safety
                /// `ptr` must point to valid MMIO memory.
                #[inline]
                pub unsafe fn read(ptr: *const Self) -> $t {
                    let val = unsafe { volatile::read::<$t>(ptr.cast()) };
                    impl_unaligned_access!(@convert val, $from)
                }
            }
        )+
    };
    (@write $t:ident, $storage:ty, $to:ident; $($access:ident),+) => {
        $(
            impl<$t: RawReg> $access<$storage> {
                /// # Safety
                /// `ptr` must point to valid, writable MMIO memory.
                #[inline]
                pub unsafe fn write(ptr: *mut Self, val: $t) {
                    let val = impl_unaligned_access!(@convert val, $to);
                    unsafe { volatile::write(ptr.cast::<$t>(), val) };
                }
            }
        )+
    };
    (@convert $value:expr, native) => { $value };
    (@convert $value:expr, $method:ident) => { $value.$method() };
}

impl_unaligned_access!(T, Unaligned<T>, native, native);
impl_unaligned_access!(T, Le<Unaligned<T>>, from_le, to_le);
impl_unaligned_access!(T, Be<Unaligned<T>>, from_be, to_be);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_wrappers_support_misaligned_buffers() {
        #[repr(align(4))]
        struct Buffer([u8; 5]);
        const VALUE: u32 = 0x1234_5678;
        let mut bytes = Buffer([0; 5]);
        let data = unsafe { bytes.0.as_mut_ptr().add(1) };
        macro_rules! check {
            ($storage:ty, $expected:expr) => {{
                unsafe { WriteOnly::<$storage>::write(data.cast(), VALUE) };
                assert_eq!(&bytes.0[1..], &$expected);
                let read = unsafe { ReadOnly::<$storage>::read(data.cast_const().cast()) };
                assert_eq!(read, VALUE);
                let read = unsafe { ReadPure::<$storage>::read(data.cast_const().cast()) };
                assert_eq!(read, VALUE);
                unsafe { ReadWrite::<$storage>::write(data.cast(), VALUE) };
                let read = unsafe { ReadWrite::<$storage>::read(data.cast_const().cast()) };
                assert_eq!(read, VALUE);
            }};
        }

        check!(Unaligned<u32>, VALUE.to_ne_bytes());
        check!(Le<Unaligned<u32>>, VALUE.to_le_bytes());
        check!(Be<Unaligned<u32>>, VALUE.to_be_bytes());
    }
}
