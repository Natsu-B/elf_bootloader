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

/// Implements one endian conversion across the supported MMIO access wrappers.
macro_rules! impl_endian {
    (
        $endian:ident($order:literal, $from:ident, $to:ident);
        readable: [$($readable:ident),+];
        writable: [$($writable:ident),+];
    ) => {
        impl<T: Copy + RawReg> $endian<T> {
            #[doc = concat!("Reads a ", $order, " value and returns it in host endianness.")]
            #[inline]
            pub fn read(&self) -> T {
                self.0.$from()
            }

            #[doc = concat!("Writes a host-endian value after converting it to ", $order, ".")]
            #[inline]
            pub fn write(&mut self, val: T) {
                self.0 = val.$to();
            }
        }

        $(
            impl<T: Copy + RawReg> Readable for $readable<$endian<T>> {
                type T = T;

                #[inline]
                fn as_ptr(&self) -> *const Self::T {
                    self.0.get().cast()
                }

                #[inline]
                fn read(&self) -> Self::T {
                    unsafe { read_volatile(&(*self.0.get()).0) }.$from()
                }
            }
        )+

        $(
            impl<T: RawReg> Writable for $writable<$endian<T>> {
                type T = T;

                #[inline]
                fn as_mut_ptr(&self) -> *mut Self::T {
                    self.0.get().cast()
                }

                #[inline]
                fn write(&self, val: Self::T) {
                    unsafe { write_volatile(&mut (*self.0.get()).0, val.$to()) };
                }
            }
        )+

        impl<T: RawReg> $endian<T> {
            #[doc = concat!("Creates a new ", $order, " wrapper from a host-endian value.")]
            pub fn new(t: T) -> Self {
                Self(t.$from())
            }
        }
    };
}

impl_endian! {
    Le("little-endian", from_le, to_le);
    readable: [ReadOnly, ReadPure, ReadWrite];
    writable: [WriteOnly, ReadWrite];
}

impl_endian! {
    Be("big-endian", from_be, to_be);
    readable: [ReadOnly, ReadPure, ReadWrite];
    writable: [WriteOnly, ReadWrite];
}

#[cfg(test)]
mod tests {
    use core::cell::UnsafeCell;

    use super::*;

    /// Verifies one wrapper's pointer and endian-conversion contracts.
    /// `encoded` is the byte-order-specific representation expected in MMIO
    /// storage after `value` is written.
    fn assert_access<E>(register: &ReadWrite<E>, value: u32, encoded: u32)
    where
        ReadWrite<E>: Readable<T = u32> + Writable<T = u32>,
    {
        // `repr(transparent)` places the raw value at the wrapper's address.
        let storage = register.0.get().cast::<u32>();
        // Both capability views must expose that same MMIO location.
        assert_eq!(register.as_ptr(), storage);
        assert_eq!(register.as_mut_ptr(), storage);

        // Exercise the writable override before inspecting device-endian storage.
        register.write(value);
        // SAFETY: `storage` points into the live `UnsafeCell` owned by `register`.
        assert_eq!(unsafe { read_volatile(storage) }, encoded);
        // The readable override converts the stored representation back to host order.
        assert_eq!(register.read(), value);
    }

    /// Covers both byte orders because target endianness decides which
    /// conversion is an identity and which swaps bytes.
    #[test]
    fn endian_access_points_to_storage_and_round_trips() {
        const VALUE: u32 = 0x1234_5678;

        assert_access(
            &ReadWrite(UnsafeCell::new(Le::new(0))),
            VALUE,
            VALUE.to_le(),
        );
        assert_access(
            &ReadWrite(UnsafeCell::new(Be::new(0))),
            VALUE,
            VALUE.to_be(),
        );
    }
}
