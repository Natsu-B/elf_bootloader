pub const PAGE_SIZE_4K: usize = 1usize << 12;
pub const PAGE_SIZE_4K_U64: u64 = PAGE_SIZE_4K as u64;

pub const fn is_aligned_usize(value: usize, align: usize) -> bool {
    if !align.is_power_of_two() {
        return false;
    }
    (value & (align - 1)) == 0
}

pub const fn align_down_usize(value: usize, align: usize) -> usize {
    if !align.is_power_of_two() {
        return value;
    }
    value & !(align - 1)
}

pub const fn align_up_usize(value: usize, align: usize) -> Option<usize> {
    if !align.is_power_of_two() {
        return None;
    }
    match value.checked_add(align - 1) {
        Some(v) => Some(v & !(align - 1)),
        None => None,
    }
}

pub const fn is_aligned_u64(value: u64, align: u64) -> bool {
    if !align.is_power_of_two() {
        return false;
    }
    (value & (align - 1)) == 0
}

pub const fn align_down_u64(value: u64, align: u64) -> u64 {
    if !align.is_power_of_two() {
        return value;
    }
    value & !(align - 1)
}

pub const fn align_up_u64(value: u64, align: u64) -> Option<u64> {
    if !align.is_power_of_two() {
        return None;
    }
    match value.checked_add(align - 1) {
        Some(v) => Some(v & !(align - 1)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_helpers_usize() {
        assert!(is_aligned_usize(0x2000, PAGE_SIZE_4K));
        assert!(!is_aligned_usize(0x2001, PAGE_SIZE_4K));
        assert_eq!(align_down_usize(0x2fff, PAGE_SIZE_4K), 0x2000);
        assert_eq!(align_up_usize(0x2001, PAGE_SIZE_4K), Some(0x3000));
        assert_eq!(align_up_usize(usize::MAX, PAGE_SIZE_4K), None);
        assert_eq!(align_up_usize(0x1000, 0), None);
    }

    #[test]
    fn align_helpers_u64() {
        assert!(is_aligned_u64(0x4000, PAGE_SIZE_4K_U64));
        assert!(!is_aligned_u64(0x4008, PAGE_SIZE_4K_U64));
        assert_eq!(align_down_u64(0x4fff, PAGE_SIZE_4K_U64), 0x4000);
        assert_eq!(align_up_u64(0x4008, PAGE_SIZE_4K_U64), Some(0x5000));
        assert_eq!(align_up_u64(u64::MAX, PAGE_SIZE_4K_U64), None);
        assert_eq!(align_up_u64(0x1000, 6), None);
    }
}
