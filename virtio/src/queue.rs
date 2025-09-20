use core::mem::size_of;
use core::sync::atomic::Ordering;

use intrusive_linked_list::IntrusiveLinkedList;
use mutex::SpinLock;
use typestate::Le;
use typestate_macro::RawReg;

use crate::VirtioErr;
use crate::cache::clean_dcache_range;
use crate::cache::invalidate_dcache_range;

#[derive(Debug)]
pub struct VirtQueue {
    size: u32,
    descriptor_paddr: u64,
    avail_paddr: u64,
    used_paddr: u64,
    idx: SpinLock<VirtQueueIdx>,
}

#[derive(Debug)]
struct VirtQueueIdx {
    free_list: IntrusiveLinkedList,
    avail_idx: u16,
    used_idx: u16,
}

#[repr(C)]
pub struct VirtqDesc {
    pub addr: Le<u64>,
    pub len: Le<u32>,
    pub flags: Le<VirtqDescFlags>,
    pub next: Le<u16>,
}

#[repr(transparent)]
#[derive(Clone, Copy, RawReg)]
pub struct VirtqDescFlags(u16);

impl VirtqDescFlags {
    // This marks a buffer as continuing via the next field
    pub const VIRTQ_DESC_F_NEXT: VirtqDescFlags = Self(1);
    // This marks a buffer as device write-only (otherwise device read-only)
    pub const VIRTQ_DESC_F_WRITE: VirtqDescFlags = Self(2);
    //  This means the buffer contains a list of buffer descriptors
    pub const VIRTQ_DESC_F_INDIRECT: VirtqDescFlags = Self(4);
}

#[repr(C)]
pub(crate) struct VirtqAvail {
    flags: Le<VirtqAvailFlags>,
    idx: Le<u16>,
    // ring: [Le<u16>; NUM_OF_DESCRIPTORS],
    // used_event: Le<u16>,
}

#[repr(transparent)]
#[derive(Clone, Copy, RawReg)]
pub(crate) struct VirtqAvailFlags(u16);

impl VirtqAvailFlags {
    const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
}

#[repr(C)]
pub(crate) struct VirtqUsed {
    flags: Le<u16>,
    idx: Le<u16>,
    // ring: [VirtqUsedElem; NUM_OF_DESCRIPTORS],
    // avail_event: Le<u16>,
}

#[repr(C)]
pub(crate) struct VirtqUsedElem {
    id: Le<u32>,
    len: Le<u32>,
}

impl VirtQueue {
    pub(crate) fn new(
        size: u32,
        descriptor_paddr: usize,
        avail_paddr: usize,
        used_paddr: usize,
    ) -> Self {
        let mut free_list = IntrusiveLinkedList::new();
        // set free list
        for i in 0..size as usize {
            unsafe { free_list.push(descriptor_paddr + i * size_of::<VirtqDesc>()) };
        }
        Self {
            size,
            descriptor_paddr: descriptor_paddr as u64,
            avail_paddr: avail_paddr as u64,
            used_paddr: used_paddr as u64,
            idx: SpinLock::new(VirtQueueIdx {
                avail_idx: 0,
                used_idx: 0,
                free_list,
            }),
        }
    }

    fn get_desc_queue(&self, desc_ptr: usize) -> (u16, &'static mut VirtqDesc) {
        let idx = (desc_ptr - self.descriptor_paddr as usize) / size_of::<VirtqDesc>();
        (idx as u16, unsafe { &mut *(desc_ptr as *mut VirtqDesc) })
    }

    fn set_avail_queue_idx(&self, avail_idx: u16, desc_idx: u16) {
        let ring_start = self.avail_paddr as usize + size_of::<VirtqAvail>();
        let slot = ring_start + avail_idx as usize * size_of::<Le<u16>>();
        unsafe {
            (&*(slot as *const Le<u16>)).write(desc_idx);
        }
        // Clean the cache line(s) containing this ring slot so device sees it.
        clean_dcache_range(slot as *const u8, size_of::<Le<u16>>());
    }

    fn get_used_queue_idx(&self, used_idx: u16) -> &'static VirtqUsedElem {
        let ring_start = self.used_paddr as usize + size_of::<VirtqUsed>();
        unsafe {
            &*((ring_start + used_idx as usize * size_of::<VirtqUsedElem>())
                as *const VirtqUsedElem)
        }
    }

    fn avail(&self) -> &'static VirtqAvail {
        unsafe { &*(self.avail_paddr as *const VirtqAvail) }
    }

    pub(crate) fn allocate_descriptor(&self) -> Result<(u16, &'static mut VirtqDesc), VirtioErr> {
        let mut lock = self.idx.lock();
        let Some(ptr) = lock.free_list.pop() else {
            return Err(VirtioErr::OutOfAvailableDesc);
        };
        Ok(self.get_desc_queue(ptr))
    }

    pub(crate) fn set_available_ring(&self, desc_idx: u16) -> Result<(), VirtioErr> {
        let mut idx = self.idx.lock();
        // Check queue fullness by tracking in-flight entries.
        let in_flight = idx.avail_idx.wrapping_sub(idx.used_idx) as u32;
        if in_flight >= self.size {
            return Err(VirtioErr::OutOfAvailableDesc);
        }

        let ring_slot = idx.avail_idx & (self.size as u16 - 1);
        self.set_avail_queue_idx(ring_slot, desc_idx);
        core::sync::atomic::fence(Ordering::Release);

        idx.avail_idx = idx.avail_idx.wrapping_add(1);
        self.avail().idx.write(idx.avail_idx);
        core::sync::atomic::fence(Ordering::Release);
        // Clean the avail header (including idx) so device observes the update.
        clean_dcache_range(self.avail_paddr as *const u8, size_of::<VirtqAvail>());
        Ok(())
    }

    pub(crate) fn pop_used(&self) -> Result<Option<(u16 /* head id */, u32 /* len */)>, VirtioErr> {
        let mut idx = self.idx.lock();
        let used_idx = idx.used_idx;
        core::sync::atomic::fence(Ordering::Acquire);

        // Invalidate used header so we see device's updates to idx
        invalidate_dcache_range(self.used_paddr as *const u8, size_of::<VirtqUsed>());
        let used = unsafe { &*(self.used_paddr as *const VirtqUsed) };
        let delta = used.idx.read().wrapping_sub(used_idx);
        if delta == 0 {
            return Ok(None);
        }
        if delta as u32 > self.size {
            return Err(VirtioErr::QueueCorrupted);
        }
        // used_idx % qsize = used_idx & (qsize - 1)
        let ring_idx = used_idx & (self.size as u16 - 1);
        // Invalidate this used ring element before reading it
        let ring_start = self.used_paddr as usize + size_of::<VirtqUsed>();
        let elem_ptr = (ring_start + ring_idx as usize * size_of::<VirtqUsedElem>()) as *const u8;
        invalidate_dcache_range(elem_ptr, size_of::<VirtqUsedElem>());
        let virt_queue_elem = self.get_used_queue_idx(ring_idx);
        idx.used_idx = used_idx.wrapping_add(1);
        Ok(Some((
            virt_queue_elem.id.read() as u16,
            virt_queue_elem.len.read(),
        )))
    }

    pub(crate) fn dequeue_used(&self, desc_idx: u16) -> Result<(), VirtioErr> {
        let mut lock = self.idx.lock();
        unsafe {
            lock.free_list
                .push(self.descriptor_paddr as usize + desc_idx as usize * size_of::<VirtqDesc>())
        };
        Ok(())
    }
}
