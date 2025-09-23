#![allow(dead_code)]

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use core::arch::asm;

    #[inline(always)]
    fn cache_line_size() -> usize {
        // CTR_EL0[19:16] = DminLine (log2 #words of 4 bytes)
        // line_size(bytes) = 4 << DminLine
        let ctr: usize;
        unsafe { asm!("mrs {ctr}, ctr_el0", ctr = out(reg) ctr) };
        let dminline = (ctr >> 16) & 0xF;
        4usize << dminline
    }

    #[inline(always)]
    fn align_range(ptr: *const u8, len: usize) -> (usize, usize, usize) {
        let line = cache_line_size();
        let start = (ptr as usize) & !(line - 1);
        // end is exclusive
        let end = (ptr as usize + len + line - 1) & !(line - 1);
        (start, end, line)
    }

    #[inline]
    pub fn clean_dcache_range(ptr: *const u8, len: usize) {
        if len == 0 {
            return;
        }
        let (mut cur, end, line) = align_range(ptr, len);
        unsafe {
            while cur < end {
                asm!("dc cvac, {addr}", addr = in(reg) cur);
                cur += line;
            }
            // Ensure completion of cache maintenance to PoC
            asm!("dsb sy");
        }
    }

    #[inline]
    pub fn invalidate_dcache_range(ptr: *const u8, len: usize) {
        if len == 0 {
            return;
        }
        let (mut cur, end, line) = align_range(ptr, len);
        unsafe {
            while cur < end {
                asm!("dc ivac, {addr}", addr = in(reg) cur);
                cur += line;
            }
            asm!("dsb sy");
        }
    }

    #[inline]
    pub fn clean_invalidate_dcache_range(ptr: *const u8, len: usize) {
        if len == 0 {
            return;
        }
        let (mut cur, end, line) = align_range(ptr, len);
        unsafe {
            while cur < end {
                asm!("dc civac, {addr}", addr = in(reg) cur);
                cur += line;
            }
            asm!("dsb sy");
        }
    }
}

#[cfg(not(target_arch = "aarch64"))]
compile_error!(
    "This module only supports the aarch64 architecture. To use it on other architectures, please provide an appropriate implementation."
);

pub use aarch64::clean_dcache_range;
pub use aarch64::clean_invalidate_dcache_range;
pub use aarch64::invalidate_dcache_range;
