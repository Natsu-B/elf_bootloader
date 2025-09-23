use alloc::alloc::alloc;
use alloc::alloc::dealloc;
use alloc::alloc::handle_alloc_error;
use alloc::slice;
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::mem::align_of;
use core::mem::size_of;
use core::num::NonZeroUsize;
use core::ops::Deref;
use core::ops::DerefMut;
use core::ptr::NonNull;
use core::ptr::{self};
use typestate::BytePod;

pub struct AlignedSliceBox<T> {
    ptr: NonNull<T>,
    len: usize,
    align: NonZeroUsize,
    _pd: PhantomData<T>,
}

impl<T> AlignedSliceBox<T> {
    pub fn new_uninit_with_align(
        len: usize,
        align: usize,
    ) -> Result<AlignedSliceBox<MaybeUninit<T>>, &'static str> {
        if size_of::<T>() == 0 || len == 0 {
            let a = NonZeroUsize::new(align_of::<T>()).unwrap();
            return Ok(AlignedSliceBox {
                ptr: NonNull::dangling(),
                len,
                align: a,
                _pd: PhantomData,
            });
        }

        let a = validate_align::<T>(align)?;
        let elem = size_of::<T>();
        let size = elem.checked_mul(len).ok_or("size overflow")?;
        let layout = Layout::from_size_align(size, a.get()).map_err(|_| "invalid layout")?;

        let raw = unsafe { alloc(layout) };
        if raw.is_null() {
            handle_alloc_error(layout);
        }

        Ok(AlignedSliceBox {
            ptr: unsafe { NonNull::new_unchecked(raw as *mut MaybeUninit<T>) },
            len,
            align: a,
            _pd: PhantomData,
        })
    }

    pub fn into_raw_parts(this: Self) -> (*mut T, usize, NonZeroUsize) {
        let ptr = this.ptr.as_ptr();
        let len = this.len;
        let align = this.align;
        core::mem::forget(this);
        (ptr, len, align)
    }
}

impl<T> AlignedSliceBox<MaybeUninit<T>> {
    pub unsafe fn assume_init(self) -> AlignedSliceBox<T> {
        let (ptr, len, align) = AlignedSliceBox::<MaybeUninit<T>>::into_raw_parts(self);
        AlignedSliceBox {
            ptr: unsafe { NonNull::new_unchecked(ptr as *mut T) },
            len,
            align,
            _pd: PhantomData,
        }
    }
}

impl<T: BytePod> AlignedSliceBox<MaybeUninit<T>> {
    pub fn deref_uninit_u8_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        let byte_len = self.len * size_of::<T>();
        let p = self.ptr.as_ptr() as *mut MaybeUninit<u8>;
        unsafe { slice::from_raw_parts_mut(p, byte_len) }
    }
}

impl<T> Deref for AlignedSliceBox<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> DerefMut for AlignedSliceBox<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for AlignedSliceBox<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AlignedSliceBox")
            .field("len", &self.len)
            .field("align", &self.align.get())
            .field("data", &&**self)
            .finish()
    }
}

unsafe impl<T: Send> Send for AlignedSliceBox<T> {}
unsafe impl<T: Sync> Sync for AlignedSliceBox<T> {}

impl<T> Drop for AlignedSliceBox<T> {
    fn drop(&mut self) {
        if size_of::<T>() == 0 || self.len == 0 {
            return;
        }
        unsafe {
            ptr::drop_in_place(core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len));
            let bytes = size_of::<T>() * self.len;
            let layout = Layout::from_size_align(bytes, self.align.get()).unwrap();
            dealloc(self.ptr.as_ptr() as *mut u8, layout);
        }
    }
}

fn validate_align<T>(align: usize) -> Result<NonZeroUsize, &'static str> {
    let a = NonZeroUsize::new(align).ok_or("align must be non-zero")?;
    if (a.get() & (a.get() - 1)) != 0 {
        return Err("align must be a power of two");
    }
    if a.get() < align_of::<T>() {
        return Err("align must be >= align_of::<T>()");
    }
    Ok(a)
}
