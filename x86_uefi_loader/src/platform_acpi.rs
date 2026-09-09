//! Read-only ACPI root discovery; MSDM payloads remain opaque.

use crate::chainload::Error;
use crate::platform_resources::MAX_MMIO_RANGES;
use crate::platform_resources::MmioMap;
use crate::platform_snapshot::MemoryMap;
use crate::platform_snapshot::malformed;
use crate::platform_snapshot::read_u32;
use crate::platform_snapshot::read_u64;
use x86_64_hal::platform_memory::PhysicalRange;
use x86_64_hal::platform_memory::PhysicalWidth;

const MAX_TABLE_ENTRIES: usize = 4096;
const MAX_ACPI_BYTES: usize = 1024 * 1024;
const ACPI_HEADER_BYTES: usize = 36;

/// Only the allowlisted MMIO table addresses survive root discovery. In
/// particular, there is no MSDM address through which a caller could read a key.
#[derive(Default)]
pub(crate) struct Tables {
    pub(crate) msdm: bool,
    pub(crate) mcfg: Option<u64>,
    pub(crate) madt: Option<u64>,
}

impl Tables {
    /// Reads only the two allowlisted resource payloads and publishes no partial
    /// MMIO list on malformed tables. This never changes either firmware table.
    pub(crate) fn mmio(&self, map: &MemoryMap<'_>, width: PhysicalWidth) -> Result<MmioMap, Error> {
        let mut output = MmioMap::empty(width)?;
        if let Some(address) = self.mcfg {
            parse_mcfg(resource_payload(map, address, b"MCFG")?, width, &mut output)?;
        }
        if let Some(address) = self.madt {
            parse_madt(resource_payload(map, address, b"APIC")?, width, &mut output)?;
        }
        Ok(output)
    }
}

/// A closed payload allowlist prevents a future caller from using this helper
/// to read MSDM or another confidential firmware table merely by changing a tag.
fn resource_payload<'a>(
    map: &'a MemoryMap<'_>,
    address: u64,
    signature: &[u8; 4],
) -> Result<&'a [u8], Error> {
    if !matches!(signature, b"MCFG" | b"APIC") {
        return Err(malformed("ACPI payload outside MMIO allowlist"));
    }
    let header = map
        .firmware_bytes(address, ACPI_HEADER_BYTES)
        .ok_or_else(|| malformed("ACPI MMIO header range"))?;
    if header.get(..4) != Some(signature.as_slice()) {
        return Err(malformed("ACPI MMIO signature changed"));
    }
    map.firmware_bytes(address, table_length(header)?)
        .ok_or_else(|| malformed("ACPI MMIO payload range"))
}

fn checked_resource_table(bytes: &[u8], signature: &[u8; 4], minimum: usize) -> Result<(), Error> {
    if bytes.len() < minimum
        || table_length(bytes)? != bytes.len()
        || bytes.get(..4) != Some(signature.as_slice())
        || !checksum_valid(bytes)
    {
        return Err(malformed("ACPI MMIO signature/length/checksum"));
    }
    Ok(())
}

/// Separate ACPI-described devices must not alias each other's register space;
/// later union with the enclosing GCD aperture is intentional and checked there.
fn add_resource(
    output: &mut MmioMap,
    range: PhysicalRange,
    width: PhysicalWidth,
) -> Result<(), Error> {
    if output
        .ranges()
        .iter()
        .any(|other| range.start() < other.end() && other.start() < range.end())
    {
        return Err(malformed("overlapping ACPI MMIO resources"));
    }
    output.insert(range, width)
}

struct McfgWindow {
    range: PhysicalRange,
    segment: u16,
    first: u8,
    last: u8,
}

fn mcfg_window(record: &[u8], width: PhysicalWidth) -> Result<McfgWindow, Error> {
    if record.len() != 16 || record[12..16] != [0; 4] || record[10] > record[11] {
        return Err(malformed("MCFG allocation layout/buses"));
    }
    let base = read_u64(record, 0).ok_or_else(|| malformed("MCFG base"))?;
    if !base.is_multiple_of(1 << 20) {
        return Err(malformed("MCFG bus-window alignment"));
    }
    // The processor-relative MCFG base describes bus zero even when the first
    // supported bus is nonzero. Do not subtract the starting bus from ECAM.
    let start = base
        .checked_add(u64::from(record[10]) << 20)
        .ok_or_else(|| malformed("MCFG start overflow"))?;
    let end = base
        .checked_add((u64::from(record[11]) + 1) << 20)
        .ok_or_else(|| malformed("MCFG end overflow"))?;
    let range = PhysicalRange::new(start, end, width)
        .map_err(|_| malformed("MCFG physical-address width"))?;
    Ok(McfgWindow {
        range,
        segment: u16::from_le_bytes([record[8], record[9]]),
        first: record[10],
        last: record[11],
    })
}

fn parse_mcfg(bytes: &[u8], width: PhysicalWidth, output: &mut MmioMap) -> Result<(), Error> {
    checked_resource_table(bytes, b"MCFG", 44)?;
    if bytes[8] != 1
        || bytes[36..44] != [0; 8]
        || bytes.len() == 44
        || !(bytes.len() - 44).is_multiple_of(16)
        || (bytes.len() - 44) / 16 > MAX_MMIO_RANGES
    {
        return Err(malformed("MCFG revision/reserved/allocation count"));
    }
    for (index, record) in bytes[44..].chunks_exact(16).enumerate() {
        let window = mcfg_window(record, width)?;
        for previous in bytes[44..44 + index * 16].chunks_exact(16) {
            let previous = mcfg_window(previous, width)?;
            if window.segment == previous.segment
                && window.first <= previous.last
                && previous.first <= window.last
            {
                return Err(malformed("overlapping MCFG segment/bus ranges"));
            }
        }
        add_resource(output, window.range, width)?;
    }
    Ok(())
}

fn apic_page(address: u64, width: PhysicalWidth) -> Result<PhysicalRange, Error> {
    let end = address
        .checked_add(4096)
        .ok_or_else(|| malformed("APIC page overflow"))?;
    PhysicalRange::new(address, end, width).map_err(|_| malformed("APIC page alignment/width"))
}

fn parse_madt(bytes: &[u8], width: PhysicalWidth, output: &mut MmioMap) -> Result<(), Error> {
    checked_resource_table(bytes, b"APIC", 44)?;
    if bytes[8] == 0 {
        return Err(malformed("MADT revision"));
    }
    let mut lapic = u64::from(read_u32(bytes, 36).ok_or_else(|| malformed("MADT LAPIC base"))?);
    let mut overridden = false;
    let mut io_ids = [0_u64; 4];
    let mut offset = 44;
    let mut entries = 0;
    while offset < bytes.len() {
        let prefix = bytes
            .get(offset..offset + 2)
            .ok_or_else(|| malformed("MADT subtable prefix"))?;
        let length = usize::from(prefix[1]);
        if length < 2 || entries >= MAX_TABLE_ENTRIES {
            return Err(malformed("MADT subtable length/count"));
        }
        let record = bytes
            .get(offset..offset + length)
            .ok_or_else(|| malformed("MADT subtable extent"))?;
        match prefix[0] {
            1 => {
                if length != 12 || record[3] != 0 {
                    return Err(malformed("MADT IOAPIC layout"));
                }
                let id = usize::from(record[2]);
                let bit = 1_u64 << (id % 64);
                if io_ids[id / 64] & bit != 0 {
                    return Err(malformed("duplicate MADT IOAPIC ID"));
                }
                io_ids[id / 64] |= bit;
                let address =
                    u64::from(read_u32(record, 4).ok_or_else(|| malformed("MADT IOAPIC base"))?);
                add_resource(output, apic_page(address, width)?, width)?;
            }
            5 => {
                if overridden || length != 12 || record[2..4] != [0; 2] {
                    return Err(malformed("MADT LAPIC override layout/count"));
                }
                lapic = read_u64(record, 4).ok_or_else(|| malformed("MADT LAPIC override base"))?;
                overridden = true;
            }
            // Extensible MADT entries not describing x86 MMIO are left intact.
            // This inventory still does not claim complete platform discovery.
            _ => {}
        }
        offset += length;
        entries += 1;
    }
    add_resource(output, apic_page(lapic, width)?, width)
}

/// Validates RSDP and root pointers, but reads only 36 bytes of each child table.
pub(crate) fn discover(address: u64, map: &MemoryMap<'_>) -> Result<Tables, Error> {
    let prefix = map
        .firmware_bytes(address, 20)
        .ok_or_else(|| malformed("ACPI RSDP range"))?;
    if prefix.get(..8) != Some(&b"RSD PTR "[..]) || !checksum_valid(prefix) {
        return Err(malformed("ACPI RSDP signature/checksum"));
    }
    let (root_address, width) = if prefix[15] >= 2 {
        let rsdp = map
            .firmware_bytes(address, 36)
            .ok_or_else(|| malformed("ACPI extended RSDP range"))?;
        let length = read_u32(rsdp, 20).ok_or_else(|| malformed("ACPI RSDP length"))? as usize;
        if !(36..=4096).contains(&length)
            || !map
                .firmware_bytes(address, length)
                .is_some_and(checksum_valid)
        {
            return Err(malformed("ACPI RSDP extended checksum/length"));
        }
        let xsdt = read_u64(rsdp, 24).ok_or_else(|| malformed("ACPI XSDT pointer"))?;
        if xsdt != 0 {
            (xsdt, 8)
        } else {
            (
                u64::from(read_u32(prefix, 16).ok_or_else(|| malformed("ACPI RSDT pointer"))?),
                4,
            )
        }
    } else {
        (
            u64::from(read_u32(prefix, 16).ok_or_else(|| malformed("ACPI RSDT pointer"))?),
            4,
        )
    };
    let header = map
        .firmware_bytes(root_address, ACPI_HEADER_BYTES)
        .ok_or_else(|| malformed("ACPI root header range"))?;
    let length = table_length(header)?;
    let root = map
        .firmware_bytes(root_address, length)
        .ok_or_else(|| malformed("ACPI root table range"))?;
    scan_root(
        root,
        width,
        |address| {
            map.firmware_bytes(address, ACPI_HEADER_BYTES)?
                .try_into()
                .ok()
        },
        |address, length| map.readable(address, length),
    )
}

/// Parses root entries with a header-only reader, keeping MSDM payload opaque.
fn scan_root(
    root: &[u8],
    width: usize,
    mut read_header: impl FnMut(u64) -> Option<[u8; ACPI_HEADER_BYTES]>,
    range_readable: impl Fn(u64, usize) -> bool,
) -> Result<Tables, Error> {
    if !matches!(width, 4 | 8)
        || root.len() < ACPI_HEADER_BYTES
        || table_length(root)? != root.len()
        || root.get(..4)
            != Some(if width == 8 {
                &b"XSDT"[..]
            } else {
                &b"RSDT"[..]
            })
        || !(root.len() - ACPI_HEADER_BYTES).is_multiple_of(width)
        || (root.len() - ACPI_HEADER_BYTES) / width > MAX_TABLE_ENTRIES
        || !checksum_valid(root)
    {
        return Err(malformed("ACPI root signature/length/checksum"));
    }
    let mut found = Tables::default();
    for offset in (ACPI_HEADER_BYTES..root.len()).step_by(width) {
        let address = if width == 8 {
            read_u64(root, offset)
        } else {
            read_u32(root, offset).map(u64::from)
        }
        .filter(|address| *address != 0)
        .ok_or_else(|| malformed("ACPI child pointer"))?;
        let header = read_header(address).ok_or_else(|| malformed("ACPI child header range"))?;
        let length = table_length(&header)?;
        if !range_readable(address, length) {
            return Err(malformed("ACPI child table range"));
        }
        let slot = match &header[..4] {
            b"MSDM" => {
                found.msdm = true;
                None
            }
            b"MCFG" => Some(&mut found.mcfg),
            b"APIC" => Some(&mut found.madt),
            _ => None,
        };
        if let Some(slot) = slot {
            if slot.replace(address).is_some() {
                return Err(malformed("duplicate ACPI MMIO table"));
            }
        }
        // Discovery still reads only headers. Full payload reads are separately
        // allowlisted to MCFG/APIC below; MSDM is never among those addresses.
    }
    Ok(found)
}

/// Checks a bounded standard ACPI header without reading the table payload.
fn table_length(header: &[u8]) -> Result<usize, Error> {
    let length = read_u32(header, 4).ok_or_else(|| malformed("ACPI table length"))? as usize;
    if header.len() < ACPI_HEADER_BYTES || !(ACPI_HEADER_BYTES..=MAX_ACPI_BYTES).contains(&length) {
        return Err(malformed("ACPI table length bound"));
    }
    Ok(length)
}

/// Applies the ACPI byte-sum checksum only to RSDP and pointer-only roots.
fn checksum_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width() -> PhysicalWidth {
        PhysicalWidth::new(48).unwrap()
    }
    fn checksum(table: &mut [u8]) {
        table[9] = 0;
        table[9] = 0_u8.wrapping_sub(table.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)));
    }
    fn table(signature: &[u8; 4], payload: &[u8]) -> std::vec::Vec<u8> {
        let mut bytes = std::vec![0; ACPI_HEADER_BYTES + payload.len()];
        bytes[..4].copy_from_slice(signature);
        let length = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        bytes[8] = 1;
        bytes[36..].copy_from_slice(payload);
        checksum(&mut bytes);
        bytes
    }
    fn allocation(base: u64, segment: u16, first: u8, last: u8) -> [u8; 16] {
        let mut result = [0; 16];
        result[..8].copy_from_slice(&base.to_le_bytes());
        result[8..10].copy_from_slice(&segment.to_le_bytes());
        result[10] = first;
        result[11] = last;
        result
    }
    fn mcfg(allocations: &[[u8; 16]]) -> std::vec::Vec<u8> {
        let mut payload = std::vec![0; 8];
        for entry in allocations {
            payload.extend_from_slice(entry);
        }
        table(b"MCFG", &payload)
    }
    fn madt(records: &[&[u8]]) -> std::vec::Vec<u8> {
        let mut payload = std::vec![0; 8];
        payload[..4].copy_from_slice(&0xfee0_0000_u32.to_le_bytes());
        for record in records {
            payload.extend_from_slice(record);
        }
        table(b"APIC", &payload)
    }
    fn override_address(address: u64) -> [u8; 12] {
        let mut result = [0; 12];
        result[0] = 5;
        result[1] = 12;
        result[4..12].copy_from_slice(&address.to_le_bytes());
        result
    }
    fn io_apic(id: u8, address: u32) -> [u8; 12] {
        let mut result = [0; 12];
        result[0] = 1;
        result[1] = 12;
        result[2] = id;
        result[4..8].copy_from_slice(&address.to_le_bytes());
        result
    }

    #[test]
    fn mcfg_nonzero_bus_high_address_and_last_physical_page() {
        let base = 1_u64 << 39;
        let bytes = mcfg(&[
            allocation(base, 0, 32, 47),
            allocation(base + (1 << 30), 1, 0, 255),
        ]);
        let mut output = MmioMap::empty(width()).unwrap();
        parse_mcfg(&bytes, width(), &mut output).unwrap();
        assert_eq!(output.ranges().len(), 2);
        assert_eq!(
            output.ranges()[0],
            PhysicalRange::new(base + (32 << 20), base + (48 << 20), width()).unwrap()
        );
        let last = mcfg_window(
            &allocation(width().limit() - (256 << 20), 0, 255, 255),
            width(),
        )
        .unwrap();
        assert_eq!(last.range.start(), width().limit() - (1 << 20));
        assert_eq!(last.range.end(), width().limit());
        assert!(
            mcfg_window(
                &allocation(width().limit() - (255 << 20), 0, 255, 255),
                width()
            )
            .is_err()
        );
        assert!(mcfg_window(&allocation(base, 0, 0, 0), PhysicalWidth::new(32).unwrap()).is_err());
    }

    #[test]
    fn mcfg_rejects_bad_tables_buses_overflow_and_conflicting_segments() {
        let valid = mcfg(&[allocation(1 << 32, 0, 0, 31)]);
        let rejects = |bytes: &[u8]| {
            parse_mcfg(bytes, width(), &mut MmioMap::empty(width()).unwrap()).is_err()
        };
        for length in 0..valid.len() {
            assert!(rejects(&valid[..length]));
        }
        for offset in [0, 8, 9, 36, 44 + 12] {
            let mut bad = valid.clone();
            bad[offset] ^= 1;
            if offset != 9 {
                checksum(&mut bad);
            }
            assert!(rejects(&bad), "offset={offset}");
        }
        for record in [
            allocation(1 << 32, 0, 32, 31),
            allocation(0x1001, 0, 0, 0),
            allocation(u64::MAX & !((1 << 20) - 1), 0, 255, 255),
        ] {
            assert!(rejects(&mcfg(&[record])));
        }
        assert!(rejects(&mcfg(&[])));
        assert!(rejects(&mcfg(
            &[allocation(1 << 32, 0, 0, 0); MAX_MMIO_RANGES + 1]
        )));
        assert!(rejects(&mcfg(&[
            allocation(1 << 32, 0, 0, 31),
            allocation(1 << 33, 0, 31, 47)
        ])));
        assert!(rejects(&mcfg(&[
            allocation(1 << 32, 0, 0, 31),
            allocation(1 << 32, 1, 0, 31)
        ])));
    }

    #[test]
    fn madt_uses_single_64_bit_override_and_discovered_io_apic() {
        let override_page = override_address(1 << 39);
        let io = io_apic(3, 0xfec0_0000);
        let cpu = [0, 8, 0, 0, 1, 0, 0, 0];
        let bytes = madt(&[&cpu, &io, &override_page]);
        let mut output = MmioMap::empty(width()).unwrap();
        parse_madt(&bytes, width(), &mut output).unwrap();
        assert_eq!(
            output.ranges(),
            &[
                PhysicalRange::new(0xfec0_0000, 0xfec0_1000, width()).unwrap(),
                PhysicalRange::new(1 << 39, (1 << 39) + 4096, width()).unwrap(),
            ]
        );
        assert!(
            output
                .ranges()
                .iter()
                .all(|range| range.start() != 0xfee0_0000)
        );
    }

    #[test]
    fn madt_rejects_bad_subtables_aliases_and_duplicate_overrides() {
        let io = io_apic(3, 0xfec0_0000);
        let override_page = override_address(1 << 39);
        let rejects = |bytes: &[u8]| {
            parse_madt(bytes, width(), &mut MmioMap::empty(width()).unwrap()).is_err()
        };
        for record in [&[1][..], &[1, 0], &[1, 1], &[1, 12], &[5, 255]] {
            assert!(rejects(&madt(&[record])));
        }
        assert!(rejects(&madt(&[&override_page, &override_page])));
        assert!(rejects(&madt(&[&io, &io_apic(3, 0xfec0_1000)])));
        assert!(rejects(&madt(&[&io, &io_apic(4, 0xfec0_0000)])));
        assert!(rejects(&madt(&[&io_apic(3, 0xfee0_0000)])));
        for address in [1, width().limit(), u64::MAX - 4095] {
            assert!(rejects(&madt(&[&override_address(address)])));
        }
        let mut bad_io = io;
        bad_io[3] = 1;
        let mut bad_override = override_page;
        bad_override[2] = 1;
        assert!(rejects(&madt(&[&bad_io])));
        assert!(rejects(&madt(&[&bad_override])));
        let mut bad_checksum = madt(&[&io]);
        bad_checksum[9] ^= 1;
        assert!(rejects(&bad_checksum));
    }

    #[test]
    fn acpi_resource_discovery_keeps_headers_only_and_rejects_duplicates() {
        for pointer_bytes in [4_usize, 8] {
            let mut payload = std::vec::Vec::new();
            for address in [0x1000_u64, 0x2000, 0x3000] {
                payload.extend_from_slice(&address.to_le_bytes()[..pointer_bytes]);
            }
            let root = table(if pointer_bytes == 8 { b"XSDT" } else { b"RSDT" }, &payload);
            // Header-only fixtures: even the test does not construct an MSDM payload.
            let mut headers = [[0_u8; ACPI_HEADER_BYTES]; 3];
            for (header, (signature, length)) in
                headers
                    .iter_mut()
                    .zip([(b"MSDM", 128_u32), (b"MCFG", 44), (b"APIC", 44)])
            {
                header[..4].copy_from_slice(signature);
                header[4..8].copy_from_slice(&length.to_le_bytes());
            }
            let mut reads = 0;
            let found = scan_root(
                &root,
                pointer_bytes,
                |address| {
                    reads += 1;
                    Some(headers[((address >> 12) - 1) as usize])
                },
                |_, _| true,
            )
            .unwrap();
            assert_eq!(reads, 3);
            assert!(found.msdm);
            assert_eq!(found.mcfg, Some(0x2000));
            assert_eq!(found.madt, Some(0x3000));
            for duplicate in [1, 2] {
                assert!(
                    scan_root(
                        &root,
                        pointer_bytes,
                        |_| Some(headers[duplicate]),
                        |_, _| true
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn preflight_msdm_discovery_reads_headers_only() {
        let mut root = [0_u8; 44];
        root[..4].copy_from_slice(b"XSDT");
        root[4..8].copy_from_slice(&44_u32.to_le_bytes());
        root[36..].copy_from_slice(&0x1000_u64.to_le_bytes());
        root[9] = 0_u8.wrapping_sub(root.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)));
        let mut header = [0_u8; ACPI_HEADER_BYTES];
        header[..4].copy_from_slice(b"MSDM");
        header[4..8].copy_from_slice(&128_u32.to_le_bytes());
        let mut reads = 0;
        let found = scan_root(
            &root,
            8,
            |address| {
                assert_eq!(address, 0x1000);
                reads += 1;
                Some(header)
            },
            |address, length| address == 0x1000 && length == 128,
        )
        .unwrap();
        assert!(found.msdm);
        assert_eq!(reads, 1);
        assert!(scan_root(&root, 8, |_| Some(header), |_, _| false).is_err());
        assert!(scan_root(&root, 8, |_| None, |_, _| true).is_err());
        root[9] ^= 1;
        assert!(scan_root(&root, 8, |_| Some(header), |_, _| true).is_err());
    }
}
