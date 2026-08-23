//! Atomic operations on raw integer types.
use core::sync::atomic::Ordering;

/// A primitive type that has a corresponding atomic type.
///
/// This trait abstracts over atomic operations on raw integer types (u8, u16, etc.),
/// allowing generic code to perform atomic loads, stores, and RMW operations.
pub trait AtomicRaw: Copy + 'static {
    /// The atomic wrapper type for this raw type.
    type Atomic;

    /// # Safety
    /// - `ptr` must be aligned to `align_of::<Self::Atomic>()`.
    /// - `ptr` must be valid for reads/writes for the returned lifetime.
    /// - Do not mix conflicting atomic and non-atomic accesses without synchronization.
    unsafe fn from_ptr<'a>(ptr: *mut Self) -> &'a Self::Atomic;

    /// Atomically loads a value from the atomic cell.
    fn load(a: &Self::Atomic, order: Ordering) -> Self;

    /// Atomically stores a value into the atomic cell.
    fn store(a: &Self::Atomic, v: Self, order: Ordering);

    /// Atomically swaps the value, returning the previous value.
    fn swap(a: &Self::Atomic, v: Self, order: Ordering) -> Self;

    /// Performs a compare-and-exchange operation.
    fn compare_exchange(
        a: &Self::Atomic,
        current: Self,
        new: Self,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Self, Self>;

    /// Performs a weak compare-and-exchange operation.
    fn compare_exchange_weak(
        a: &Self::Atomic,
        current: Self,
        new: Self,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Self, Self>;

    /// Atomically performs bitwise OR and returns the previous value.
    fn fetch_or(a: &Self::Atomic, v: Self, order: Ordering) -> Self;

    /// Atomically performs bitwise AND and returns the previous value.
    fn fetch_and(a: &Self::Atomic, v: Self, order: Ordering) -> Self;

    /// Atomically performs bitwise XOR and returns the previous value.
    fn fetch_xor(a: &Self::Atomic, v: Self, order: Ordering) -> Self;
}

/// Atomic operations for integer types with arithmetic operations.
pub trait AtomicRawInt: AtomicRaw {
    /// Atomically adds to the current value, returning the previous value.
    fn fetch_add(a: &Self::Atomic, v: Self, order: Ordering) -> Self;

    /// Atomically subtracts from the current value, returning the previous value.
    fn fetch_sub(a: &Self::Atomic, v: Self, order: Ordering) -> Self;

    /// Atomically computes the minimum, returning the previous value.
    fn fetch_min(a: &Self::Atomic, v: Self, order: Ordering) -> Self;

    /// Atomically computes the maximum, returning the previous value.
    fn fetch_max(a: &Self::Atomic, v: Self, order: Ordering) -> Self;

    /// Atomically performs bitwise NAND, returning the previous value.
    fn fetch_nand(a: &Self::Atomic, v: Self, order: Ordering) -> Self;
}

macro_rules! impl_atomic_raw {
    ($raw:ty, $atomic:ty) => {
        impl AtomicRaw for $raw {
            type Atomic = $atomic;

            #[inline(always)]
            unsafe fn from_ptr<'a>(ptr: *mut Self) -> &'a Self::Atomic {
                // SAFETY: caller upholds the Atomic*::from_ptr contract.
                unsafe { <$atomic>::from_ptr(ptr) }
            }

            #[inline(always)]
            fn load(a: &Self::Atomic, order: Ordering) -> Self {
                a.load(order)
            }

            #[inline(always)]
            fn store(a: &Self::Atomic, v: Self, order: Ordering) {
                a.store(v, order)
            }

            #[inline(always)]
            fn swap(a: &Self::Atomic, v: Self, order: Ordering) -> Self {
                a.swap(v, order)
            }

            #[inline(always)]
            fn compare_exchange(
                a: &Self::Atomic,
                current: Self,
                new: Self,
                success: Ordering,
                failure: Ordering,
            ) -> Result<Self, Self> {
                a.compare_exchange(current, new, success, failure)
            }

            #[inline(always)]
            fn compare_exchange_weak(
                a: &Self::Atomic,
                current: Self,
                new: Self,
                success: Ordering,
                failure: Ordering,
            ) -> Result<Self, Self> {
                a.compare_exchange_weak(current, new, success, failure)
            }

            #[inline(always)]
            fn fetch_or(a: &Self::Atomic, v: Self, order: Ordering) -> Self {
                a.fetch_or(v, order)
            }

            #[inline(always)]
            fn fetch_and(a: &Self::Atomic, v: Self, order: Ordering) -> Self {
                a.fetch_and(v, order)
            }

            #[inline(always)]
            fn fetch_xor(a: &Self::Atomic, v: Self, order: Ordering) -> Self {
                a.fetch_xor(v, order)
            }
        }
    };
}

macro_rules! impl_atomic_raw_int {
    ($raw:ty, $atomic:ty) => {
        impl_atomic_raw!($raw, $atomic);
        impl AtomicRawInt for $raw {
            #[inline(always)]
            fn fetch_add(a: &Self::Atomic, v: Self, order: Ordering) -> Self {
                a.fetch_add(v, order)
            }

            #[inline(always)]
            fn fetch_sub(a: &Self::Atomic, v: Self, order: Ordering) -> Self {
                a.fetch_sub(v, order)
            }

            #[inline(always)]
            fn fetch_min(a: &Self::Atomic, v: Self, order: Ordering) -> Self {
                a.fetch_min(v, order)
            }

            #[inline(always)]
            fn fetch_max(a: &Self::Atomic, v: Self, order: Ordering) -> Self {
                a.fetch_max(v, order)
            }

            #[inline(always)]
            fn fetch_nand(a: &Self::Atomic, v: Self, order: Ordering) -> Self {
                a.fetch_nand(v, order)
            }
        }
    };
}

#[cfg(target_has_atomic = "8")]
mod impl8 {
    use super::*;
    use core::sync::atomic::AtomicBool;
    use core::sync::atomic::AtomicI8;
    use core::sync::atomic::AtomicU8;

    impl_atomic_raw!(bool, AtomicBool);
    impl_atomic_raw_int!(u8, AtomicU8);
    impl_atomic_raw_int!(i8, AtomicI8);
}

#[cfg(target_has_atomic = "16")]
mod impl16 {
    use super::*;
    use core::sync::atomic::AtomicI16;
    use core::sync::atomic::AtomicU16;

    impl_atomic_raw_int!(u16, AtomicU16);
    impl_atomic_raw_int!(i16, AtomicI16);
}

#[cfg(target_has_atomic = "32")]
mod impl32 {
    use super::*;
    use core::sync::atomic::AtomicI32;
    use core::sync::atomic::AtomicU32;

    impl_atomic_raw_int!(u32, AtomicU32);
    impl_atomic_raw_int!(i32, AtomicI32);
}

#[cfg(target_has_atomic = "64")]
mod impl64 {
    use super::*;
    use core::sync::atomic::AtomicI64;
    use core::sync::atomic::AtomicU64;

    impl_atomic_raw_int!(u64, AtomicU64);
    impl_atomic_raw_int!(i64, AtomicI64);
}

#[cfg(target_has_atomic = "ptr")]
mod implptr {
    use super::*;
    use core::sync::atomic::AtomicIsize;
    use core::sync::atomic::AtomicUsize;

    impl_atomic_raw_int!(usize, AtomicUsize);
    impl_atomic_raw_int!(isize, AtomicIsize);
}
