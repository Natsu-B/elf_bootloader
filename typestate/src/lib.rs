#![cfg_attr(not(test), no_std)]
//! MMIO typestate wrapper.
//!
//! This crate provides small wrapper types around MMIO registers that encode
//! readable / writable capabilities at the type level (typestate).
//!
//! Volatile memory access is performed **only** by the access-capability
//! wrappers (`ReadOnly`/`ReadPure`/`WriteOnly`/`ReadWrite`). Composition
//! wrappers (`Le<T>`/`Be<T>`/`Unaligned<T>`) adapt value representation and
//! delegate actual MMIO to the access wrappers, ensuring a clear separation
//! of responsibilities.
//!
//! # Typestates
//! - [`ReadOnly<T>`]: readable, no write API is exposed. Reads **may** have side effects.
//! - [`ReadPure<T>`]: readable **without side effects** (safe to poll). Volatile
//!   reads are still performed, but only via the access wrapper.
//! - [`WriteOnly<T>`]: writable, no read API is exposed.
//! - [`ReadWrite<T>`]: both readable and writable.
//! - [`Le<T>`] / [`Be<T>`]: endianness-aware wrappers that convert to host endianness.
//! - [`Unaligned<T>`]: unaligned access helper that performs byte-wise I/O via access wrappers.
//!
//! # Bitfield Helpers
//! - [`bitregs!`]: declarative macro for defining MMIO register layouts with
//!   compile-time coverage and overlap checks, available via
//!   [`crate::bitregs!`](crate::bitregs!) or the alias [`crate::bitflags!`](crate::bitflags!).
//!
//! # Safety
//! These wrappers do not validate that the underlying address actually maps to
//! device registers. It is **your** responsibility to place these wrappers at
//! the correct, valid MMIO address and to follow the device's access rules.

pub mod bitflags;
mod endianness;
mod read_write;
mod unaligned;

pub use endianness::Be;
pub use endianness::Le;
pub use read_write::ReadOnly;
pub use read_write::ReadPure;
pub use read_write::ReadWrite;
pub use read_write::Readable;
pub use read_write::Writable;
pub use read_write::WriteOnly;
pub use unaligned::Unaligned;

pub unsafe trait RawReg:
    Copy + core::ops::BitOr + core::ops::BitAnd + core::ops::Not + core::ops::BitXor
{
    type Raw;
    fn to_raw(self) -> Self::Raw;
    fn from_raw(raw: Self::Raw) -> Self;
    fn to_le(self) -> Self;
    fn from_le(self) -> Self;
    fn to_be(self) -> Self;
    fn from_be(self) -> Self;
}

pub unsafe trait BytePod: Copy + 'static {}

macro_rules! impl_raw { ($($t:ty),* $(,)?) => {$(
    unsafe impl RawReg for $t {
        type Raw = $t;
        #[inline] fn to_raw(self) -> Self::Raw {self}
        #[inline] fn from_raw(raw: Self::Raw) -> Self {raw}
        #[inline] fn to_le(self)->Self{Self::to_le(self)}
        #[inline] fn from_le(self)->Self{Self::from_le(self)}
        #[inline] fn to_be(self)->Self{Self::to_be(self)}
        #[inline] fn from_be(self)->Self{Self::from_be(self)}
    }
    unsafe impl BytePod for $t {}
)*}}
impl_raw!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize
);

unsafe impl<T: BytePod> BytePod for Le<T> {}
unsafe impl<T: BytePod> BytePod for Be<T> {}
