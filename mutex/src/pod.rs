//! POD-backed atomic cells for bring-up friendly atomic operations.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::align_of;
use core::mem::size_of;
use core::num::Wrapping;
use core::sync::atomic::Ordering;

use typestate::AtomicPod;
use typestate::RawReg;
use typestate::atomic_raw::AtomicRaw;
use typestate::atomic_raw::AtomicRawInt;

// ===== RawAtomicPod =======================================================
//
// A bring-up friendly atomic cell:
// - Before enable_raw_atomics(): non-atomic plain memory access (single-core bring-up).
// - After enable_raw_atomics(): atomic operations via Atomic*::from_ptr().
//
// This is intentionally "raw": enable_raw_atomics() is not synchronized and must be
// called before introducing concurrent access.

/// A "raw" atomic POD that starts as non-atomic until atomics are enabled.
///
/// - Before `enable_raw_atomics()`: operations are performed non-atomically (intended for single-core bring-up).
/// - After `enable_raw_atomics()`: operations use Atomic* on the same storage without requiring IRQ masking.
///
/// # Safety / Invariants
/// - `enable_raw_atomics()` must be called only during a phase where no concurrent accesses exist.
/// - Using these operations concurrently before raw atomics are enabled is unsupported and can
///   cause data races or aliasing UB.
/// - After `enable_raw_atomics()`, you must not perform conflicting non-atomic accesses to the same value.
///   (This includes taking `&mut` to the underlying storage while other cores use atomics.)
pub struct RawAtomicPod<T: AtomicPod>
where
    T::Raw: AtomicRaw,
{
    raw: UnsafeCell<T::Raw>,
    _phantom: PhantomData<T>,
}

/// Alias used by crates that refer to raw atomics as byte-pod backed cells.
pub type RawBytePod<T> = RawAtomicPod<T>;

unsafe impl<T: AtomicPod> Sync for RawAtomicPod<T> where T::Raw: AtomicRaw + Send {}
unsafe impl<T: AtomicPod> Send for RawAtomicPod<T> where T::Raw: AtomicRaw + Send {}

impl<T: AtomicPod> RawAtomicPod<T>
where
    T::Raw: AtomicRaw,
{
    const _LAYOUT_OK: () = {
        // Atomic* types guarantee same size/bit validity as the underlying scalar.
        // AtomicBool additionally guarantees same alignment as bool. :contentReference[oaicite:6]{index=6}
        // AtomicU32 guarantees same size, but alignment may be larger than u32 on some targets. :contentReference[oaicite:7]{index=7}
        // We enforce Raw's alignment is sufficient for its Atomic mapping.
        assert!(size_of::<T::Raw>() == size_of::<<T::Raw as AtomicRaw>::Atomic>());
        assert!(align_of::<T::Raw>() >= align_of::<<T::Raw as AtomicRaw>::Atomic>());
    };

    /// Creates a new cell from a raw value, canonicalizing it first.
    #[inline]
    pub fn new_raw(init: T::Raw) -> Self {
        let () = Self::_LAYOUT_OK;
        Self {
            raw: UnsafeCell::new(T::canonicalize_raw(init)),
            _phantom: PhantomData,
        }
    }

    /// Creates a new cell from a raw value without canonicalization.
    ///
    /// # Safety
    /// Caller must ensure `init` is already canonical for `T`.
    #[inline]
    pub const unsafe fn new_raw_unchecked(init: T::Raw) -> Self {
        let () = Self::_LAYOUT_OK;
        Self {
            raw: UnsafeCell::new(init),
            _phantom: PhantomData,
        }
    }

    /// Creates a new cell from a typed value.
    #[inline]
    pub fn new(init: T) -> Self {
        let () = Self::_LAYOUT_OK;
        Self {
            raw: UnsafeCell::new(T::canonicalize_raw(init.to_raw())),
            _phantom: PhantomData,
        }
    }

    /// Returns a reference to the underlying atomic.
    #[inline(always)]
    fn atomic_ref(&self) -> &<T::Raw as AtomicRaw>::Atomic {
        let ptr = self.raw.get();
        debug_assert!(ptr.cast::<<T::Raw as AtomicRaw>::Atomic>().is_aligned());
        // SAFETY: `raw` is properly aligned by construction (_LAYOUT_OK),
        // and the returned reference is bounded by `&self`.
        unsafe { <T::Raw as AtomicRaw>::from_ptr(ptr) }
    }

    /// Applies a read-modify-write operation during the non-atomic bring-up phase.
    ///
    /// # Exclusivity
    ///
    /// Callers select this path only while raw atomics are disabled. The phase's
    /// exclusivity invariant makes the plain load and store one logical update.
    /// The update closure runs once within that exclusive interval.
    ///
    /// # Ordering
    ///
    /// No ordering is applied because this mode provides no synchronization.
    ///
    /// # Return value
    ///
    /// The original raw value is converted after the store to match `fetch_*`.
    #[inline(always)]
    fn non_atomic_fetch_update(&self, update: impl FnOnce(T::Raw) -> T::Raw) -> T {
        // SAFETY: Non-atomic bring-up phase must ensure exclusive access externally.
        let old = unsafe { *self.raw.get() };
        // SAFETY: The same exclusive access covers the matching write.
        unsafe { *self.raw.get() = update(old) };
        T::from_raw(old)
    }

    /// Loads the value with the specified ordering.
    #[inline]
    pub fn load(&self, order: Ordering) -> T {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(T::canonicalize_raw(<T::Raw as AtomicRaw>::load(a, order)))
        } else {
            // SAFETY: Non-atomic bring-up phase must ensure exclusive access externally.
            T::from_raw(T::canonicalize_raw(unsafe { *self.raw.get() }))
        }
    }

    /// Stores the value with the specified ordering.
    #[inline]
    pub fn store(&self, val: T, order: Ordering) {
        let val_raw = T::canonicalize_raw(val.to_raw());
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            <T::Raw as AtomicRaw>::store(a, val_raw, order);
        } else {
            // SAFETY: Non-atomic bring-up phase must ensure exclusive access externally.
            unsafe { *self.raw.get() = val_raw };
        }
    }

    /// Atomically swaps the value, returning the previous value.
    #[inline]
    pub fn swap(&self, val: T, order: Ordering) -> T {
        let val_raw = T::canonicalize_raw(val.to_raw());
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(T::canonicalize_raw(<T::Raw as AtomicRaw>::swap(
                a, val_raw, order,
            )))
        } else {
            // SAFETY: Non-atomic bring-up phase must ensure exclusive access externally.
            let old = unsafe { *self.raw.get() };
            // SAFETY: Non-atomic bring-up phase must ensure exclusive access externally.
            unsafe { *self.raw.get() = val_raw };
            T::from_raw(T::canonicalize_raw(old))
        }
    }

    /// Returns a mutable reference to the raw storage.
    #[inline]
    pub fn get_mut_raw(&mut self) -> &mut T::Raw {
        // This is always safe because &mut self guarantees exclusivity.
        //
        // Callers must only write canonical values. If non-canonical values are written,
        // CAS stability is not guaranteed until the value is repaired by a CAS path.
        self.raw.get_mut()
    }
}

impl<T: RawReg + 'static> RawAtomicPod<T>
where
    T::Raw: AtomicRaw,
{
    /// Atomically performs bitwise OR and returns the previous value.
    #[inline]
    pub fn fetch_or(&self, val: T, order: Ordering) -> T
    where
        T::Raw: core::ops::BitOr<Output = T::Raw>,
    {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(<T::Raw as AtomicRaw>::fetch_or(a, val.to_raw(), order))
        } else {
            self.non_atomic_fetch_update(|old| old | val.to_raw())
        }
    }

    /// Atomically performs bitwise AND and returns the previous value.
    #[inline]
    pub fn fetch_and(&self, val: T, order: Ordering) -> T
    where
        T::Raw: core::ops::BitAnd<Output = T::Raw>,
    {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(<T::Raw as AtomicRaw>::fetch_and(a, val.to_raw(), order))
        } else {
            self.non_atomic_fetch_update(|old| old & val.to_raw())
        }
    }

    /// Atomically performs bitwise XOR and returns the previous value.
    #[inline]
    pub fn fetch_xor(&self, val: T, order: Ordering) -> T
    where
        T::Raw: core::ops::BitXor<Output = T::Raw>,
    {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(<T::Raw as AtomicRaw>::fetch_xor(a, val.to_raw(), order))
        } else {
            self.non_atomic_fetch_update(|old| old ^ val.to_raw())
        }
    }
}

impl<T: RawReg + 'static> RawAtomicPod<T>
where
    T::Raw: AtomicRawInt,
{
    /// Atomically adds to the current value, returning the previous value.
    #[inline]
    pub fn fetch_add(&self, val: T, order: Ordering) -> T
    where
        T::Raw: core::ops::Add<Output = T::Raw>,
        Wrapping<T::Raw>: core::ops::Add<Output = Wrapping<T::Raw>>,
    {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(<T::Raw as AtomicRawInt>::fetch_add(a, val.to_raw(), order))
        } else {
            self.non_atomic_fetch_update(|old| (Wrapping(old) + Wrapping(val.to_raw())).0)
        }
    }

    /// Atomically subtracts from the current value, returning the previous value.
    #[inline]
    pub fn fetch_sub(&self, val: T, order: Ordering) -> T
    where
        T::Raw: core::ops::Sub<Output = T::Raw>,
        Wrapping<T::Raw>: core::ops::Sub<Output = Wrapping<T::Raw>>,
    {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(<T::Raw as AtomicRawInt>::fetch_sub(a, val.to_raw(), order))
        } else {
            self.non_atomic_fetch_update(|old| (Wrapping(old) - Wrapping(val.to_raw())).0)
        }
    }

    /// Atomically multiplies the current value, returning the previous value.
    #[inline]
    pub fn fetch_mul(&self, val: T, order: Ordering) -> T
    where
        T::Raw: core::ops::Mul<Output = T::Raw>,
        Wrapping<T::Raw>: core::ops::Mul<Output = Wrapping<T::Raw>>,
    {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            let failure = match order {
                Ordering::AcqRel => Ordering::Acquire,
                Ordering::Release => Ordering::Relaxed,
                _ => order,
            };
            let val_raw = val.to_raw();
            let mut old = <T::Raw as AtomicRaw>::load(a, Ordering::Relaxed);
            loop {
                let new = (Wrapping(old) * Wrapping(val_raw)).0;
                match <T::Raw as AtomicRaw>::compare_exchange_weak(a, old, new, order, failure) {
                    Ok(prev) => return T::from_raw(prev),
                    Err(prev) => old = prev,
                }
            }
        } else {
            self.non_atomic_fetch_update(|old| (Wrapping(old) * Wrapping(val.to_raw())).0)
        }
    }

    /// Atomically computes the minimum, returning the previous value.
    #[inline]
    pub fn fetch_min(&self, val: T, order: Ordering) -> T
    where
        T::Raw: Ord,
    {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(<T::Raw as AtomicRawInt>::fetch_min(a, val.to_raw(), order))
        } else {
            self.non_atomic_fetch_update(|old| core::cmp::min(old, val.to_raw()))
        }
    }

    /// Atomically computes the maximum, returning the previous value.
    #[inline]
    pub fn fetch_max(&self, val: T, order: Ordering) -> T
    where
        T::Raw: Ord,
    {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(<T::Raw as AtomicRawInt>::fetch_max(a, val.to_raw(), order))
        } else {
            self.non_atomic_fetch_update(|old| core::cmp::max(old, val.to_raw()))
        }
    }

    /// Atomically performs bitwise NAND, returning the previous value.
    #[inline]
    pub fn fetch_nand(&self, val: T, order: Ordering) -> T
    where
        T::Raw: core::ops::BitAnd<Output = T::Raw> + core::ops::Not<Output = T::Raw>,
    {
        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();
            T::from_raw(<T::Raw as AtomicRawInt>::fetch_nand(a, val.to_raw(), order))
        } else {
            self.non_atomic_fetch_update(|old| !(old & val.to_raw()))
        }
    }
}

impl<T: AtomicPod> RawAtomicPod<T>
where
    T::Raw: AtomicRaw + PartialEq,
{
    /// Performs a compare-and-exchange operation.
    #[inline]
    pub fn compare_exchange(
        &self,
        current: T,
        new: T,
        success: Ordering,
        failure: Ordering,
    ) -> Result<T, T> {
        let cur = T::canonicalize_raw(current.to_raw());
        let new = T::canonicalize_raw(new.to_raw());

        if crate::raw_atomics_enabled() {
            let a = self.atomic_ref();

            let mut observed = <T::Raw as AtomicRaw>::load(a, Ordering::Relaxed);
            loop {
                let observed_can = T::canonicalize_raw(observed);
                if observed == observed_can {
                    break;
                }
                match <T::Raw as AtomicRaw>::compare_exchange_weak(
                    a,
                    observed,
                    observed_can,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(next) => observed = next,
                }
            }

            <T::Raw as AtomicRaw>::compare_exchange(a, cur, new, success, failure)
                .map(|prev| T::from_raw(T::canonicalize_raw(prev)))
                .map_err(|prev| T::from_raw(T::canonicalize_raw(prev)))
        } else {
            // SAFETY: Non-atomic bring-up phase must ensure exclusive access externally.
            let stored = unsafe { *self.raw.get() };
            let stored_can = T::canonicalize_raw(stored);
            if stored != stored_can {
                // SAFETY: Non-atomic bring-up phase must ensure exclusive access externally.
                unsafe { *self.raw.get() = stored_can };
            }

            if stored_can == cur {
                // SAFETY: Non-atomic bring-up phase must ensure exclusive access externally.
                unsafe { *self.raw.get() = new };
                Ok(T::from_raw(stored_can))
            } else {
                Err(T::from_raw(stored_can))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr::NonNull;
    use core::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::thread;

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    #[repr(transparent)]
    struct MaskedU32(u32);

    unsafe impl typestate::AtomicPod for MaskedU32 {
        type Raw = u32;

        #[inline]
        fn to_raw(self) -> Self::Raw {
            self.0
        }

        #[inline]
        fn from_raw(raw: Self::Raw) -> Self {
            Self(raw)
        }

        #[inline]
        fn canonicalize_raw(raw: Self::Raw) -> Self::Raw {
            raw & 0xFFFF_FF00
        }
    }

    #[test]
    fn raw_atomic_pod_non_atomic_fetch_add_wraps() {
        crate::with_raw_atomics_mode(false, || {
            let pod = RawAtomicPod::new(0xFFu8);
            let prev = pod.fetch_add(1u8, Ordering::Relaxed);
            assert_eq!(prev, 0xFF);
            assert_eq!(pod.load(Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn raw_atomic_pod_non_atomic_fetch_bitwise() {
        crate::with_raw_atomics_mode(false, || {
            let pod = RawAtomicPod::new(0b1010u8);
            assert_eq!(pod.fetch_or(0b0101, Ordering::Relaxed), 0b1010);
            assert_eq!(pod.fetch_and(0b1100, Ordering::Relaxed), 0b1111);
            assert_eq!(pod.fetch_xor(0b1010, Ordering::Relaxed), 0b1100);
            assert_eq!(pod.load(Ordering::Relaxed), 0b0110);
        });
    }

    #[test]
    fn raw_atomic_pod_non_atomic_fetch_min_max_signed() {
        crate::with_raw_atomics_mode(false, || {
            let pod = RawAtomicPod::new(10i32);
            let prev = pod.fetch_min(3i32, Ordering::Relaxed);
            assert_eq!(prev, 10);
            assert_eq!(pod.load(Ordering::Relaxed), 3);
            let prev = pod.fetch_max(20i32, Ordering::Relaxed);
            assert_eq!(prev, 3);
            assert_eq!(pod.load(Ordering::Relaxed), 20);
        });
    }

    #[test]
    fn raw_atomic_pod_non_atomic_fetch_nand() {
        crate::with_raw_atomics_mode(false, || {
            let old = 0b1100u8;
            let val = 0b1010u8;
            let pod = RawAtomicPod::new(old);
            let prev = pod.fetch_nand(val, Ordering::Relaxed);
            assert_eq!(prev, old);
            assert_eq!(pod.load(Ordering::Relaxed), !(old & val));
        });
    }

    #[test]
    fn raw_atomic_pod_atomic_fetch_add_multithreaded() {
        crate::with_raw_atomics_mode(true, || {
            let pod = Arc::new(RawAtomicPod::new(0u32));
            let threads = 4usize;
            let iters = 1_000usize;
            let mut handles = Vec::with_capacity(threads);

            for _ in 0..threads {
                let pod = Arc::clone(&pod);
                handles.push(thread::spawn(move || {
                    for _ in 0..iters {
                        pod.fetch_add(1u32, Ordering::Relaxed);
                    }
                }));
            }

            for handle in handles {
                handle.join().unwrap();
            }

            assert_eq!(pod.load(Ordering::Relaxed), (threads * iters) as u32);
        });
    }

    #[test]
    fn raw_atomic_pod_canonicalizes_store_and_compare_exchange() {
        for enabled in [false, true] {
            crate::with_raw_atomics_mode(enabled, || {
                let pod = RawAtomicPod::new(MaskedU32(0x1234_5678));
                assert_eq!(pod.load(Ordering::Relaxed), MaskedU32(0x1234_5600));

                pod.store(MaskedU32(0xABCD_EF12), Ordering::Relaxed);
                assert_eq!(pod.load(Ordering::Relaxed), MaskedU32(0xABCD_EF00));

                let result = pod.compare_exchange(
                    MaskedU32(0xABCD_EFFF),
                    MaskedU32(0xCAFE_BA7F),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                assert_eq!(result, Ok(MaskedU32(0xABCD_EF00)));
                assert_eq!(pod.load(Ordering::Relaxed), MaskedU32(0xCAFE_BA00));
            });
        }
    }

    #[cfg(target_has_atomic = "ptr")]
    #[test]
    fn raw_atomic_pod_option_nonnull_compare_exchange() {
        for enabled in [false, true] {
            crate::with_raw_atomics_mode(enabled, || {
                let pod = RawAtomicPod::new(None::<NonNull<u8>>);
                let mut x = 42u8;
                let ptr = Some(NonNull::from(&mut x));

                let result = pod.compare_exchange(None, ptr, Ordering::AcqRel, Ordering::Acquire);
                assert_eq!(result, Ok(None));
                assert_eq!(pod.load(Ordering::Relaxed), ptr);
            });
        }
    }
}
