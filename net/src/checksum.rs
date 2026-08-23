//! Internet checksum (RFC 1071) calculation utilities.

/// Computes the raw one's-complement sum over a byte slice.
///
/// For odd lengths, the final byte is treated as the high byte of a 16-bit word.
pub fn ones_complement_sum(data: &[u8]) -> u32 {
    let (words, tail) = data.as_chunks::<2>();
    let sum = words.iter().fold(0u32, |sum, word| {
        sum.wrapping_add(u32::from(u16::from_be_bytes(*word)))
    });
    tail.first()
        .map_or(sum, |byte| sum.wrapping_add(u32::from(*byte) << 8))
}

/// Folds a 32-bit accumulator into a 16-bit one's-complement sum.
pub fn fold_ones_complement_sum(sum: u32) -> u16 {
    let sum = (sum & 0xFFFF) + (sum >> 16);
    ((sum & 0xFFFF) + (sum >> 16)) as u16
}

/// Computes the IPv4 header checksum value to be written into the header field.
pub fn ipv4_header_checksum(header: &[u8]) -> u16 {
    !fold_ones_complement_sum(ones_complement_sum(header))
}

/// Validates an IPv4 header checksum.
///
/// Returns `true` when the folded one's-complement sum equals `0xFFFF`.
pub fn ipv4_header_checksum_is_valid(header: &[u8]) -> bool {
    fold_ones_complement_sum(ones_complement_sum(header)) == 0xFFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_odd_length_and_folds_carry() {
        assert_eq!(ones_complement_sum(&[0x12, 0x34, 0x56]), 0x6834);
        assert_eq!(fold_ones_complement_sum(u32::MAX), u16::MAX);
    }
}
