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
        $endian:ident($from:ident, $to:ident);
        readable: [$($readable:ident),+];
        writable: [$($writable:ident),+];
        read: $read_doc:literal;
        write: $write_doc:literal;
        new: $new_doc:literal;
    ) => {
        impl<T: Copy + RawReg> $endian<T> {
            #[doc = $read_doc]
            #[inline]
            pub fn read(&self) -> T {
                self.0.$from()
            }

            #[doc = $write_doc]
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
                    unreachable!()
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

                fn as_mut_ptr(&self) -> *mut Self::T {
                    unreachable!()
                }

                #[inline]
                fn write(&self, val: Self::T) {
                    unsafe { write_volatile(&mut (*self.0.get()).0, val.$to()) };
                }
            }
        )+

        impl<T: RawReg> $endian<T> {
            #[doc = $new_doc]
            pub fn new(t: T) -> Self {
                Self(t.$from())
            }
        }
    };
}

impl_endian! {
    Le(from_le, to_le);
    readable: [ReadOnly, ReadPure, ReadWrite];
    writable: [WriteOnly, ReadWrite];
    read: "Reads a little-endian value and returns it in host endianness.";
    write: "Writes a host-endian value after converting it to little-endian.";
    new: "Creates a new little-endian wrapper from a host-endian value.";
}

impl_endian! {
    Be(from_be, to_be);
    readable: [ReadOnly, ReadPure, ReadWrite];
    writable: [WriteOnly, ReadWrite];
    read: "Reads a big-endian value and returns it in host endianness.";
    write: "Writes a host-endian value after converting it to big-endian.";
    new: "Creates a new big-endian wrapper from a host-endian value.";
}
