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

/// Generates direct delegations to the matching standard atomic methods.
macro_rules! delegate_atomic {
    ($(fn $method:ident($($arg:ident: $ty:ty),*) -> $result:ty;)+) => {
        $(
            #[inline(always)]
            fn $method(a: &Self::Atomic, $($arg: $ty),*) -> $result {
                a.$method($($arg),*)
            }
        )+
    };
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

            delegate_atomic! {
                fn load(order: Ordering) -> Self;
                fn store(v: Self, order: Ordering) -> ();
                fn swap(v: Self, order: Ordering) -> Self;
                fn compare_exchange(
                    current: Self, new: Self, success: Ordering, failure: Ordering
                ) -> Result<Self, Self>;
                fn compare_exchange_weak(
                    current: Self, new: Self, success: Ordering, failure: Ordering
                ) -> Result<Self, Self>;
                fn fetch_or(v: Self, order: Ordering) -> Self;
                fn fetch_and(v: Self, order: Ordering) -> Self;
                fn fetch_xor(v: Self, order: Ordering) -> Self;
            }
        }
    };
}

macro_rules! impl_atomic_raw_int {
    ($raw:ty, $atomic:ty) => {
        impl_atomic_raw!($raw, $atomic);
        impl AtomicRawInt for $raw {
            delegate_atomic! {
                fn fetch_add(v: Self, order: Ordering) -> Self;
                fn fetch_sub(v: Self, order: Ordering) -> Self;
                fn fetch_min(v: Self, order: Ordering) -> Self;
                fn fetch_max(v: Self, order: Ordering) -> Self;
                fn fetch_nand(v: Self, order: Ordering) -> Self;
            }
        }
    };
}

#[cfg(target_has_atomic = "8")]
impl_atomic_raw!(bool, core::sync::atomic::AtomicBool);
#[cfg(target_has_atomic = "8")]
impl_atomic_raw_int!(u8, core::sync::atomic::AtomicU8);
#[cfg(target_has_atomic = "8")]
impl_atomic_raw_int!(i8, core::sync::atomic::AtomicI8);

#[cfg(target_has_atomic = "16")]
impl_atomic_raw_int!(u16, core::sync::atomic::AtomicU16);
#[cfg(target_has_atomic = "16")]
impl_atomic_raw_int!(i16, core::sync::atomic::AtomicI16);

#[cfg(target_has_atomic = "32")]
impl_atomic_raw_int!(u32, core::sync::atomic::AtomicU32);
#[cfg(target_has_atomic = "32")]
impl_atomic_raw_int!(i32, core::sync::atomic::AtomicI32);

#[cfg(target_has_atomic = "64")]
impl_atomic_raw_int!(u64, core::sync::atomic::AtomicU64);
#[cfg(target_has_atomic = "64")]
impl_atomic_raw_int!(i64, core::sync::atomic::AtomicI64);

#[cfg(target_has_atomic = "ptr")]
impl_atomic_raw_int!(usize, core::sync::atomic::AtomicUsize);
#[cfg(target_has_atomic = "ptr")]
impl_atomic_raw_int!(isize, core::sync::atomic::AtomicIsize);
