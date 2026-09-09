//! Checked copying of this project's already firmware-loaded PE32+ image.
//!
//! This is not a disk PE loader: firmware has already authenticated, loaded and
//! relocated the source. The independent copy must never be registered as a
//! firmware runtime image, because its pointers belong to physical L0 HOST_CR3.

use crate::platform_snapshot::read_u32;
use crate::platform_snapshot::read_u64;

const PAGE: u64 = 4096;
const MAX_IMAGE: usize = 16 * 1024 * 1024;
const MAX_RELOCATIONS: usize = 65536;

/// A malformed or unsupported loaded project image; no destination is published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Header,
    Unsupported,
    Address,
    Section,
    Relocation,
    Capacity,
}

/// Validated immutable source, borrowed only before VMX/guest execution begins.
pub(crate) struct LoadedPe<'a> {
    bytes: &'a [u8],
    base: u64,
    headers: usize,
    sections: usize,
    section_count: usize,
    reloc_start: usize,
    reloc_end: usize,
}

fn half(bytes: &[u8], offset: usize) -> Option<u16> {
    let word = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([word[0], word[1]]))
}

/// Bounds required by the current four-level, low-canonical L0 identity map.
fn image_end(base: u64, bytes: usize) -> Result<u64, Error> {
    if base == 0 || !base.is_multiple_of(PAGE) || bytes == 0 || bytes > MAX_IMAGE {
        return Err(Error::Address);
    }
    base.checked_add(bytes as u64)
        .filter(|&end| end <= 1 << 47)
        .ok_or(Error::Address)
}

/// Bounds the allocation before constructing any slice over loaded metadata.
pub(crate) fn image_pages(base: u64, bytes: u64) -> Result<usize, Error> {
    let bytes = usize::try_from(bytes).map_err(|_| Error::Address)?;
    image_end(base, bytes)?;
    if !bytes.is_multiple_of(PAGE as usize) {
        return Err(Error::Address);
    }
    Ok(bytes / PAGE as usize)
}

/// Private payload surrounded by two explicit L1-visible, zeroed boot pages.
/// Linux copy_bootdata copies COMMAND_LINE_SIZE (2048) bytes even when its EFI
/// command-line allocation is shorter and adjacent to a runtime reservation.
/// These guards allow that bounded read without exposing any L0-owned payload.
/// ponytail: one page covers this verified 2 KiB overread, not arbitrary scans;
/// broader guest accesses must be diagnosed, never remap private state on fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GuardedAllocation {
    pub(crate) base: u64,
    pub(crate) payload: u64,
    pub(crate) payload_end: u64,
    pub(crate) end: u64,
}

impl GuardedAllocation {
    /// Validates the complete allocation, including the shared bookend pages.
    pub(crate) fn new(base: u64, payload_pages: usize, limit: u64) -> Result<Self, Error> {
        if base == 0
            || !base.is_multiple_of(PAGE)
            || payload_pages == 0
            || payload_pages > MAX_IMAGE / PAGE as usize
            || !limit.is_power_of_two()
            || !(1 << 32..=1 << 47).contains(&limit)
        {
            return Err(Error::Address);
        }
        let end = base
            .checked_add((payload_pages as u64 + 2) * PAGE)
            .filter(|&end| end <= limit)
            .ok_or(Error::Address)?;
        Ok(Self {
            base,
            payload: base + PAGE,
            payload_end: end - PAGE,
            end,
        })
    }
}

impl<'a> LoadedPe<'a> {
    /// Checks all metadata and DIR64 fixups before any destination write.
    /// `bytes` is the complete loaded virtual layout, not raw file sections.
    pub(crate) fn parse(bytes: &'a [u8], base: u64) -> Result<Self, Error> {
        let end = image_end(base, bytes.len())?;
        if !bytes.len().is_multiple_of(PAGE as usize) || bytes.get(..2) != Some(b"MZ") {
            return Err(Error::Header);
        }
        let pe = read_u32(bytes, 0x3c).ok_or(Error::Header)? as usize;
        let signature_end = pe.checked_add(4).ok_or(Error::Header)?;
        if pe < 0x40 || bytes.get(pe..signature_end) != Some(b"PE\0\0") {
            return Err(Error::Header);
        }
        let coff = signature_end;
        let section_count = usize::from(half(bytes, coff + 2).ok_or(Error::Header)?);
        let optional_bytes = usize::from(half(bytes, coff + 16).ok_or(Error::Header)?);
        let characteristics = half(bytes, coff + 18).ok_or(Error::Header)?;
        if half(bytes, coff) != Some(0x8664)
            || !(1..=96).contains(&section_count)
            || optional_bytes < 240
            || characteristics & 1 != 0
        {
            return Err(Error::Unsupported);
        }
        let optional = coff.checked_add(20).ok_or(Error::Header)?;
        let sections = optional.checked_add(optional_bytes).ok_or(Error::Header)?;
        let table_end = sections
            .checked_add(section_count * 40)
            .ok_or(Error::Header)?;
        if table_end > bytes.len() || half(bytes, optional) != Some(0x20b) {
            return Err(Error::Header);
        }
        let headers = read_u32(bytes, optional + 60).ok_or(Error::Header)? as usize;
        if headers < table_end
            || headers > bytes.len()
            || read_u32(bytes, optional + 56) != Some(bytes.len() as u32)
            || read_u32(bytes, optional + 32) != Some(PAGE as u32)
            || read_u32(bytes, optional + 108) != Some(16)
            || !matches!(half(bytes, optional + 68), Some(10 | 12))
        {
            return Err(Error::Unsupported);
        }
        // The no_std project has no imports, TLS or delayed imports. A copied
        // image cannot silently retain loader-managed state for these features.
        for directory in [1, 9, 13] {
            if read_u64(bytes, optional + 112 + directory * 8) != Some(0) {
                return Err(Error::Unsupported);
            }
        }
        let reloc_start = read_u32(bytes, optional + 112 + 5 * 8).ok_or(Error::Header)? as usize;
        let reloc_bytes = read_u32(bytes, optional + 116 + 5 * 8).ok_or(Error::Header)? as usize;
        let reloc_end = reloc_start
            .checked_add(reloc_bytes)
            .ok_or(Error::Relocation)?;
        if reloc_start < headers
            || reloc_bytes < 8
            || reloc_end > bytes.len()
            || !reloc_start.is_multiple_of(4)
            || !reloc_bytes.is_multiple_of(4)
        {
            return Err(Error::Relocation);
        }
        let image = Self {
            bytes,
            base,
            headers,
            sections,
            section_count,
            reloc_start,
            reloc_end,
        };
        let mut previous_end = headers;
        for index in 0..section_count {
            let (start, limit, _) = image.section(index);
            if start < previous_end
                || !start.is_multiple_of(PAGE as usize)
                || start == limit
                || limit > bytes.len()
            {
                return Err(Error::Section);
            }
            previous_end = limit;
        }
        if !image.contains_section(reloc_start, reloc_end, false) {
            return Err(Error::Relocation);
        }
        let entry = read_u32(bytes, optional + 16).ok_or(Error::Header)? as usize;
        if !image.contains_section(entry, entry.checked_add(1).ok_or(Error::Address)?, true) {
            return Err(Error::Section);
        }
        let mut cursor = reloc_start;
        let mut previous = 0;
        let mut count = 0_usize;
        while cursor < reloc_end {
            let page = read_u32(bytes, cursor).ok_or(Error::Relocation)? as usize;
            let size = read_u32(bytes, cursor + 4).ok_or(Error::Relocation)? as usize;
            let next = cursor.checked_add(size).ok_or(Error::Relocation)?;
            if size < 8
                || !size.is_multiple_of(4)
                || next > reloc_end
                || !page.is_multiple_of(PAGE as usize)
            {
                return Err(Error::Relocation);
            }
            cursor += 8;
            while cursor < next {
                let record = half(bytes, cursor).ok_or(Error::Relocation)?;
                cursor += 2;
                if record >> 12 == 0 {
                    continue;
                }
                if record >> 12 != 10 {
                    return Err(Error::Unsupported);
                }
                count += 1;
                let target = page
                    .checked_add(usize::from(record & 4095))
                    .ok_or(Error::Relocation)?;
                let limit = target.checked_add(8).ok_or(Error::Relocation)?;
                // Require ordered nonoverlapping project fixups, never a header
                // or a fixup that rewrites the relocation directory itself.
                if count > MAX_RELOCATIONS
                    || target < previous
                    || target < headers
                    || (target < reloc_end && reloc_start < limit)
                    || !image.contains_section(target, limit, false)
                {
                    return Err(Error::Relocation);
                }
                let value = read_u64(bytes, target).ok_or(Error::Relocation)?;
                // A one-past-image pointer is legal, but external absolute
                // pointers are not silently shifted by this project copier.
                if value < base || value > end {
                    return Err(Error::Address);
                }
                previous = limit;
            }
        }
        if count == 0 {
            return Err(Error::Relocation);
        }
        Ok(image)
    }

    /// Section arithmetic is widened; parse checked the entire fixed table.
    fn section(&self, index: usize) -> (usize, usize, u32) {
        let row = &self.bytes[self.sections + index * 40..self.sections + (index + 1) * 40];
        let word = |offset| {
            u32::from_le_bytes([
                row[offset],
                row[offset + 1],
                row[offset + 2],
                row[offset + 3],
            ])
        };
        let start = word(12) as usize;
        (start, start + word(8).max(word(16)) as usize, word(36))
    }

    fn contains_section(&self, start: usize, end: usize, executable: bool) -> bool {
        (0..self.section_count).any(|index| {
            let (base, limit, flags) = self.section(index);
            base <= start
                && start < end
                && end <= limit
                && flags & 0x4000_0000 != 0
                && (!executable || flags & 0x2000_0000 != 0)
        })
    }

    /// Returns a copied internal function only if its source is executable code.
    pub(crate) fn entry(&self, destination: u64, source_entry: u64) -> Result<u64, Error> {
        image_end(destination, self.bytes.len())?;
        let rva = source_entry.checked_sub(self.base).ok_or(Error::Address)?;
        let rva = usize::try_from(rva).map_err(|_| Error::Address)?;
        if rva < self.headers
            || !self.contains_section(rva, rva.checked_add(1).ok_or(Error::Address)?, true)
        {
            return Err(Error::Address);
        }
        destination.checked_add(rva as u64).ok_or(Error::Address)
    }

    /// Checks a naturally aligned shared bootstrap atomic in writable image data.
    pub(crate) fn bootstrap_word(&self, address: u64) -> bool {
        let Some(offset) = address
            .checked_sub(self.base)
            .and_then(|rva| usize::try_from(rva).ok())
        else {
            return false;
        };
        let Some(end) = offset.checked_add(8) else {
            return false;
        };
        address.is_multiple_of(8)
            && (0..self.section_count).any(|index| {
                let (start, limit, flags) = self.section(index);
                start <= offset && end <= limit && flags & 0xe000_0000 == 0xc000_0000
            })
    }

    /// Copies and rebases only the immutable source's validated DIR64 slots.
    /// All recoverable failures precede writes; bytes beyond the image stay intact.
    /// Actual source/destination ownership and mappings belong to the caller.
    pub(crate) fn copy_to(&self, output: &mut [u8], destination: u64) -> Result<(), Error> {
        let end = image_end(destination, self.bytes.len())?;
        if destination < self.base + self.bytes.len() as u64 && self.base < end {
            return Err(Error::Address);
        }
        if output.len() < self.bytes.len() {
            return Err(Error::Capacity);
        }
        output[..self.bytes.len()].copy_from_slice(self.bytes);
        let mut cursor = self.reloc_start;
        while cursor < self.reloc_end {
            // Parse proved every field/range and source immutability prevents
            // those invariants changing between validation and materialization.
            let word = |offset| {
                u32::from_le_bytes([
                    self.bytes[offset],
                    self.bytes[offset + 1],
                    self.bytes[offset + 2],
                    self.bytes[offset + 3],
                ]) as usize
            };
            let page = word(cursor);
            let next = cursor + word(cursor + 4);
            cursor += 8;
            while cursor < next {
                let record = u16::from_le_bytes([self.bytes[cursor], self.bytes[cursor + 1]]);
                cursor += 2;
                if record >> 12 == 0 {
                    continue;
                }
                let target = page + usize::from(record & 4095);
                let mut bytes = [0; 8];
                bytes.copy_from_slice(&self.bytes[target..target + 8]);
                let value = destination + (u64::from_le_bytes(bytes) - self.base);
                output[target..target + 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x20_0000;
    const OPTIONAL: usize = 0x98;
    const SECTIONS: usize = OPTIONAL + 240;

    fn put16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn fixture() -> [u8; 0x4000] {
        let mut bytes = [0; 0x4000];
        bytes[..2].copy_from_slice(b"MZ");
        put32(&mut bytes, 0x3c, 0x80);
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        put16(&mut bytes, 0x84, 0x8664);
        put16(&mut bytes, 0x86, 3);
        put16(&mut bytes, 0x94, 240);
        put16(&mut bytes, OPTIONAL, 0x20b);
        put32(&mut bytes, OPTIONAL + 16, 0x1000);
        put32(&mut bytes, OPTIONAL + 32, 4096);
        put32(&mut bytes, OPTIONAL + 56, 0x4000);
        put32(&mut bytes, OPTIONAL + 60, 0x400);
        put16(&mut bytes, OPTIONAL + 68, 12);
        put32(&mut bytes, OPTIONAL + 108, 16);
        put32(&mut bytes, OPTIONAL + 152, 0x3000);
        put32(&mut bytes, OPTIONAL + 156, 16);
        for (index, flags) in [0x6000_0000, 0xc000_0000, 0x4200_0000]
            .into_iter()
            .enumerate()
        {
            let row = SECTIONS + index * 40;
            put32(&mut bytes, row + 8, 4096);
            put32(&mut bytes, row + 12, (index as u32 + 1) * 4096);
            put32(&mut bytes, row + 16, 4096);
            put32(&mut bytes, row + 36, flags);
        }
        put64(&mut bytes, 0x2000, BASE + 0x1000);
        put64(&mut bytes, 0x2008, BASE + 0x4000);
        put64(&mut bytes, 0x2010, 0xfeed_beef);
        put32(&mut bytes, 0x3000, 0x2000);
        put32(&mut bytes, 0x3004, 16);
        put16(&mut bytes, 0x3008, 0xa000);
        put16(&mut bytes, 0x300a, 0xa008);
        bytes
    }

    #[test]
    fn loaded_project_copy_rebases_both_directions_and_keeps_non_fixups() {
        let source = fixture();
        let pe = LoadedPe::parse(&source, BASE).unwrap();
        for destination in [0x10_0000, 1 << 34, (1 << 47) - 0x4000] {
            let mut output = [0x5a; 0x4008];
            pe.copy_to(&mut output, destination).unwrap();
            assert_eq!(read_u64(&output, 0x2000), Some(destination + 0x1000));
            assert_eq!(read_u64(&output, 0x2008), Some(destination + 0x4000));
            assert_eq!(read_u64(&output, 0x2010), Some(0xfeed_beef));
            assert_eq!(
                pe.entry(destination, BASE + 0x1001),
                Ok(destination + 0x1001)
            );
            assert_eq!(&output[0x4000..], &[0x5a; 8]);
        }
        assert_eq!(source, fixture());
        assert!(pe.bootstrap_word(BASE + 0x2020));
        for offset in [0x2001, 0x2ffc, 0x1000, 0x3000, 0x4000] {
            assert!(!pe.bootstrap_word(BASE + offset));
        }
        assert_eq!(pe.entry(0x10_0000, BASE + 0x2000), Err(Error::Address));
        assert_eq!(pe.entry(0x10_0000, 0), Err(Error::Address));
    }

    #[test]
    fn loaded_project_copy_rejects_bounds_before_writing() {
        let source = fixture();
        let pe = LoadedPe::parse(&source, BASE).unwrap();
        for destination in [
            0,
            1,
            BASE,
            BASE - 4096,
            BASE + 4096,
            1 << 47,
            u64::MAX - 4095,
        ] {
            let mut output = [0x5a; 0x4000];
            assert_eq!(pe.copy_to(&mut output, destination), Err(Error::Address));
            assert_eq!(output, [0x5a; 0x4000]);
        }
        let mut short = [0x5a; 8];
        assert_eq!(pe.copy_to(&mut short, 0x10_0000), Err(Error::Capacity));
        assert_eq!(short, [0x5a; 8]);
        assert_eq!(image_pages(BASE, 0x4000), Ok(4));
        for (base, size) in [
            (0, 4096),
            (1, 4096),
            (BASE, 0),
            (BASE, 4097),
            (BASE, MAX_IMAGE as u64 + 4096),
            ((1 << 47) - 4096, 8192),
            (BASE, u64::MAX),
        ] {
            assert_eq!(image_pages(base, size), Err(Error::Address));
        }
    }

    #[test]
    fn boot_guards_are_disjoint_from_every_private_payload_page() {
        let region = GuardedAllocation::new(BASE, 601, 1 << 47).unwrap();
        assert_eq!(region.payload, BASE + 4096);
        assert_eq!(region.payload_end - region.payload, 601 * 4096);
        assert_eq!(region.end - region.payload_end, 4096);
        // Any 2048-byte forward copy starting in the preceding allocation ends
        // in the shared guard, before the VMXON/private PE payload starts.
        for distance in 1..=2048 {
            assert!(region.base - distance + 2048 < region.payload);
        }
        assert!(
            GuardedAllocation::new(
                (1 << 47) - (MAX_IMAGE as u64 + 8192),
                MAX_IMAGE / 4096,
                1 << 47
            )
            .is_ok()
        );
        for (base, pages, limit) in [
            (0, 1, 1 << 47),
            (BASE + 1, 1, 1 << 47),
            (BASE, 0, 1 << 47),
            (BASE, MAX_IMAGE / 4096 + 1, 1 << 47),
            (BASE, usize::MAX, 1 << 47),
            ((1 << 32) - 8192, 1, 1 << 32),
            (BASE, 1, 1 << 48),
            (BASE, 1, (1 << 47) - 1),
            (u64::MAX - 4095, 1, 1 << 47),
        ] {
            assert_eq!(
                GuardedAllocation::new(base, pages, limit),
                Err(Error::Address)
            );
        }
    }

    #[test]
    fn loaded_project_copy_rejects_malformed_metadata_and_hidden_loader_state() {
        for (offset, value) in [
            (0x3c, u32::MAX),
            (OPTIONAL + 16, 0x2000),
            (OPTIONAL + 32, 512),
            (OPTIONAL + 56, 8192),
            (OPTIONAL + 60, 1),
            (OPTIONAL + 108, 17),
            (OPTIONAL + 120, 1),
            (OPTIONAL + 184, 1),
            (OPTIONAL + 216, 1),
            (OPTIONAL + 152, u32::MAX),
            (OPTIONAL + 156, u32::MAX),
            (SECTIONS + 12, 4097),
            (SECTIONS + 40 + 12, 4096),
            (SECTIONS + 40 + 8, u32::MAX),
            (SECTIONS + 36, 0x4000_0000),
            (0x3004, 0),
            (0x3004, 7),
            (0x3004, 9),
            (0x3004, 4096),
            (0x3000, 0x2001),
            (0x3000, 0x3000),
            (0x3000, 0),
        ] {
            let mut bytes = fixture();
            put32(&mut bytes, offset, value);
            assert!(
                LoadedPe::parse(&bytes, BASE).is_err(),
                "offset {offset:#x} value {value:#x}"
            );
        }
        for (offset, value) in [
            (0x84, 0xaa64),
            (0x86, 0),
            (0x86, 97),
            (0x94, 239),
            (0x96, 1),
            (OPTIONAL, 0x10b),
            (OPTIONAL + 68, 11),
            (0x3008, 0x3000),
            (0x300a, 0xa000),
            (0x300a, 0xa004),
            (0x300a, 0xafff),
        ] {
            let mut bytes = fixture();
            put16(&mut bytes, offset, value);
            assert!(
                LoadedPe::parse(&bytes, BASE).is_err(),
                "offset {offset:#x} value {value:#x}"
            );
        }
        for value in [0, BASE - 1, BASE + 0x4001, u64::MAX] {
            let mut bytes = fixture();
            put64(&mut bytes, 0x2000, value);
            assert!(matches!(LoadedPe::parse(&bytes, BASE), Err(Error::Address)));
        }
        assert!(LoadedPe::parse(&[], BASE).is_err());
        let mut bytes = fixture();
        bytes[0] = 0;
        assert!(matches!(LoadedPe::parse(&bytes, BASE), Err(Error::Header)));
        let mut bytes = fixture();
        bytes[0x3008..0x3010].fill(0);
        assert!(matches!(
            LoadedPe::parse(&bytes, BASE),
            Err(Error::Relocation)
        ));
    }
}
