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
//! - [`bitregs!`]: macro for defining MMIO register layouts with compile-time
//!   coverage and overlap checks.
//!
//! # Safety
//! These wrappers do not validate that the underlying address actually maps to
//! device registers. It is **your** responsibility to place these wrappers at
//! the correct, valid MMIO address and to follow the device's access rules.

#[cfg(test)]
extern crate self as typestate;

/// Atomic operations on raw integer types.
pub mod atomic_raw;
/// Bitfield register definition macros.
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

#[doc(hidden)]
pub use typestate_macro::bitregs_impl as __bitregs_impl;

/// # Safety
/// Implementors must ensure:
/// - `to_raw()` and `from_raw()` perform lossless round-trip conversions.
/// - The type is `Copy` and supports bitwise operations safely.
/// - Endianness conversions are correct for the underlying type.
pub unsafe trait RawReg:
    Copy + core::ops::BitOr + core::ops::BitAnd + core::ops::Not + core::ops::BitXor
{
    /// The underlying raw representation type.
    type Raw;
    /// Converts `self` to the raw representation.
    fn to_raw(self) -> Self::Raw;
    /// Constructs `Self` from its raw representation.
    fn from_raw(raw: Self::Raw) -> Self;
    /// Converts `self` to little-endian byte order.
    fn to_le(self) -> Self;
    /// Interprets `self` as little-endian and converts to native byte order.
    fn from_le(self) -> Self;
    /// Converts `self` to big-endian byte order.
    fn to_be(self) -> Self;
    /// Interprets `self` as big-endian and converts to native byte order.
    fn from_be(self) -> Self;
}

/// # Safety
/// Implementors must ensure all bit patterns are valid for the type.
pub unsafe trait BytePod: Copy + 'static {}

/// A POD-like value that can be represented in an atomic raw storage slot.
///
/// # Safety
/// Implementations must ensure:
/// - All values stored into atomic cells are canonicalized first.
/// - `canonicalize_raw` may clear invalid/unused bits by mapping them to 0.
/// - `from_raw(Self::canonicalize_raw(raw))` is always safe for any `raw`.
/// - `to_raw()` produces a representation suitable for atomic storage in `Self::Raw`.
pub unsafe trait AtomicPod: Copy + 'static {
    /// The underlying atomic-compatible raw type.
    type Raw: crate::atomic_raw::AtomicRaw;

    /// Converts `self` to the raw representation for atomic storage.
    fn to_raw(self) -> Self::Raw;
    /// Constructs `Self` from its raw atomic representation.
    fn from_raw(raw: Self::Raw) -> Self;

    /// Clears invalid/unused bits, returning a canonical representation.
    #[inline]
    fn canonicalize_raw(raw: Self::Raw) -> Self::Raw {
        raw
    }
}

/// # Safety
/// Implementors must be 8-bit atomic-compatible types.
pub unsafe trait U8: AtomicPod<Raw = u8> {}
/// # Safety
/// Implementors must be 16-bit atomic-compatible types.
pub unsafe trait U16: AtomicPod<Raw = u16> {}
/// # Safety
/// Implementors must be 32-bit atomic-compatible types.
pub unsafe trait U32: AtomicPod<Raw = u32> {}
/// # Safety
/// Implementors must be 64-bit atomic-compatible types.
pub unsafe trait U64: AtomicPod<Raw = u64> {}

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

unsafe impl RawReg for bool {
    type Raw = bool;

    fn to_raw(self) -> Self::Raw {
        self
    }

    fn from_raw(raw: Self::Raw) -> Self {
        raw
    }

    fn to_le(self) -> Self {
        self
    }

    fn from_le(self) -> Self {
        self
    }

    fn to_be(self) -> Self {
        self
    }

    fn from_be(self) -> Self {
        self
    }

    // NOTE: Do NOT implement BytePod for bool.
    // bool does not allow arbitrary bit patterns (only 0/1 are valid).
}

unsafe impl<T: BytePod> BytePod for Le<T> {}
unsafe impl<T: BytePod> BytePod for Be<T> {}

unsafe impl<T> AtomicPod for T
where
    T: RawReg + 'static,
    T::Raw: crate::atomic_raw::AtomicRaw,
{
    type Raw = T::Raw;

    #[inline]
    fn to_raw(self) -> Self::Raw {
        RawReg::to_raw(self)
    }

    #[inline]
    fn from_raw(raw: Self::Raw) -> Self {
        RawReg::from_raw(raw)
    }
}

#[cfg(target_has_atomic = "ptr")]
// SAFETY: `Option<NonNull<T>>` is represented as zero/non-zero pointer values.
// Non-zero pointers may be dangling; dereference safety and provenance validity
// are managed by the higher-level publish/init-once + CAS protocol.
unsafe impl<T: 'static> AtomicPod for Option<core::ptr::NonNull<T>> {
    type Raw = usize;

    #[inline]
    fn to_raw(self) -> Self::Raw {
        match self {
            None => 0,
            Some(ptr) => ptr.as_ptr() as usize,
        }
    }

    #[inline]
    fn from_raw(raw: Self::Raw) -> Self {
        if raw == 0 {
            None
        } else {
            // SAFETY: non-zero integers can represent non-null pointer values.
            // The pointer may be dangling; dereference validity is external.
            Some(unsafe { core::ptr::NonNull::new_unchecked(raw as *mut T) })
        }
    }

    #[inline]
    fn canonicalize_raw(raw: Self::Raw) -> Self::Raw {
        // TODO: apply target-specific address validity policy if needed.
        raw
    }
}

#[cfg(target_has_atomic = "8")]
unsafe impl U8 for u8 {}

#[cfg(target_has_atomic = "16")]
unsafe impl U16 for u16 {}

#[cfg(target_has_atomic = "32")]
unsafe impl U32 for u32 {}

#[cfg(target_has_atomic = "64")]
unsafe impl U64 for u64 {}

#[cfg(test)]
mod tests {
    use super::AtomicPod;

    #[derive(Clone, Copy, Debug, Eq, PartialEq, typestate_macro::RawReg)]
    #[repr(transparent)]
    struct TestReg(u32);

    /// Transparent wrapper used to verify derive-time canonicalization masks.
    /// Only its low 16 bits are valid in atomic storage.
    #[derive(Clone, Copy, typestate_macro::U32)]
    #[atomic_pod(mask = 0x0000_FFFF)]
    #[repr(transparent)]
    struct MaskedU32(u32);

    #[cfg(target_has_atomic = "64")]
    #[derive(Copy, Clone, typestate_macro::U64)]
    #[repr(C)]
    struct PaddedU64 {
        a: u8,
        b: u32,
    }

    #[cfg(target_has_atomic = "64")]
    fn is_field_byte(byte: usize) -> bool {
        let a_off = core::mem::offset_of!(PaddedU64, a);
        let b_off = core::mem::offset_of!(PaddedU64, b);
        byte == a_off || (b_off..(b_off + core::mem::size_of::<u32>())).contains(&byte)
    }

    #[cfg(target_has_atomic = "64")]
    #[test]
    fn derive_u64_to_raw_zeroes_padding_bytes() {
        let value = PaddedU64 {
            a: 0xA5,
            b: 0x1122_3344,
        };
        let raw = <PaddedU64 as AtomicPod>::to_raw(value);
        let bytes = raw.to_ne_bytes();

        let a_off = core::mem::offset_of!(PaddedU64, a);
        let b_off = core::mem::offset_of!(PaddedU64, b);

        assert_eq!(bytes[a_off], 0xA5);
        assert_eq!(bytes[b_off..(b_off + 4)], 0x1122_3344u32.to_ne_bytes());
        for (idx, byte) in bytes.iter().enumerate() {
            if !is_field_byte(idx) {
                assert_eq!(*byte, 0);
            }
        }
    }

    #[cfg(target_has_atomic = "64")]
    #[test]
    fn derive_u64_canonicalize_clears_padding_bytes() {
        let a_off = core::mem::offset_of!(PaddedU64, a);
        let b_off = core::mem::offset_of!(PaddedU64, b);

        let mut bytes = [0xFFu8; 8];
        bytes[a_off] = 0x5A;
        bytes[b_off..(b_off + 4)].copy_from_slice(&0x8877_6655u32.to_ne_bytes());

        let raw = u64::from_ne_bytes(bytes);
        let canon = <PaddedU64 as AtomicPod>::canonicalize_raw(raw);
        let canon_bytes = canon.to_ne_bytes();

        assert_eq!(canon_bytes[a_off], 0x5A);
        assert_eq!(
            canon_bytes[b_off..(b_off + 4)],
            0x8877_6655u32.to_ne_bytes()
        );
        for (idx, byte) in canon_bytes.iter().enumerate() {
            if !is_field_byte(idx) {
                assert_eq!(*byte, 0);
            }
        }
    }

    #[cfg(target_has_atomic = "64")]
    #[test]
    fn derive_u64_from_raw_ignores_noncanonical_padding() {
        let a_off = core::mem::offset_of!(PaddedU64, a);
        let b_off = core::mem::offset_of!(PaddedU64, b);

        let mut bytes = [0xABu8; 8];
        bytes[a_off] = 0x31;
        bytes[b_off..(b_off + 4)].copy_from_slice(&0x0102_0304u32.to_ne_bytes());

        let raw = u64::from_ne_bytes(bytes);
        let value = <PaddedU64 as AtomicPod>::from_raw(raw);
        assert_eq!(value.a, 0x31);
        assert_eq!(value.b, 0x0102_0304);

        let repacked = <PaddedU64 as AtomicPod>::to_raw(value);
        let repacked_bytes = repacked.to_ne_bytes();
        for (idx, byte) in repacked_bytes.iter().enumerate() {
            if !is_field_byte(idx) {
                assert_eq!(*byte, 0);
            }
        }
    }

    #[test]
    fn derive_rawreg_delegates_all_operators() {
        let lhs = TestReg(12);
        let rhs = TestReg(5);
        assert_eq!(lhs | rhs, TestReg(13));
        assert_eq!(lhs & rhs, TestReg(4));
        assert_eq!(lhs ^ rhs, TestReg(9));
        assert_eq!(lhs + rhs, TestReg(17));
        assert_eq!(lhs - rhs, TestReg(7));
        assert_eq!(lhs * rhs, TestReg(60));
        assert_eq!(lhs / rhs, TestReg(2));
        assert_eq!(lhs % rhs, TestReg(2));
        assert_eq!(!lhs, TestReg(!12));

        let mut value = lhs;
        value |= rhs;
        value &= TestReg(7);
        value ^= TestReg(3);
        value += TestReg(4);
        value -= TestReg(2);
        value *= TestReg(3);
        value /= TestReg(4);
        value %= TestReg(4);
        assert_eq!(value, TestReg(2));
    }

    /// Confirms that packing and unpacking apply the same raw-value mask.
    #[test]
    fn derive_u32_transparent_mask_canonicalizes_raw() {
        // Both conversion directions must produce a canonical representation.
        assert_eq!(
            <MaskedU32 as AtomicPod>::to_raw(MaskedU32(u32::MAX)),
            0xFFFF
        );
        assert_eq!(<MaskedU32 as AtomicPod>::from_raw(u32::MAX).0, 0xFFFF);
    }
}
