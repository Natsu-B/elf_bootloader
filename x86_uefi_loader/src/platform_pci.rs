//! Read-only UEFI PCI root apertures and assigned BAR cross-checks.
//!
//! UEFI 2.11 sections 14.2.18 and 14.4.18: resource minima are host
//! addresses; translation offsets convert them to PCI addresses, not EPT GPAs.

use crate::SerialPort;
use crate::chainload;
use crate::chainload::Error;
use crate::platform_resources::MAX_MMIO_RANGES;
use crate::platform_resources::MmioMap;
use crate::platform_snapshot::MemoryMap;
use crate::platform_snapshot::malformed;
use crate::platform_snapshot::read_u32;
use crate::platform_snapshot::read_u64;
use core::ffi::c_void;
use core::fmt::Write;
use core::mem;
use core::ptr;
use core::slice;
use r_efi::efi;
use r_efi::protocols::pci_io;
use x86_64_hal::platform_memory::PhysicalRange;
use x86_64_hal::platform_memory::PhysicalWidth;

const ROOT_GUID: efi::Guid = efi::Guid::from_fields(
    0x2f707ebb,
    0x4a1a,
    0x11d4,
    0x9a,
    0x38,
    &[0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d],
);
const ROOT_CONFIGURATION: usize = 136;
const ROOT_SEGMENT: usize = 144;
const MAX_ROOTS: usize = 32;
const MAX_HANDLES: usize = 4096;
const QWORD_BYTES: usize = 46;
const PAGE: u64 = 4096;

type Configuration = unsafe extern "efiapi" fn(*mut c_void, *mut *mut c_void) -> efi::Status;

#[derive(Clone, Copy, Default)]
struct Resource {
    kind: u8,
    start: u64,
    end: u64,
    translation: u64,
}

impl Resource {
    fn page_range(self, width: PhysicalWidth) -> Result<PhysicalRange, Error> {
        let end = self
            .end
            .checked_add(PAGE - 1)
            .ok_or_else(|| malformed("PCI resource page overflow"))?
            & !(PAGE - 1);
        PhysicalRange::new(self.start & !(PAGE - 1), end, width)
            .map_err(|_| malformed("PCI resource physical width"))
    }
}

fn resource(bytes: &[u8], bar: bool, width: PhysicalWidth) -> Result<Resource, Error> {
    if bytes.len() != QWORD_BYTES
        || bytes[..3] != [0x8a, 0x2b, 0]
        || bytes[3] > 2
        || bytes[4] & !0x0f != 0
    {
        return Err(malformed("PCI QWORD resource header"));
    }
    let field = |offset| read_u64(bytes, offset).ok_or_else(|| malformed("PCI resource field"));
    let kind = bytes[3];
    let granularity = field(6)?;
    let start = field(14)?;
    let maximum = field(22)?;
    let translation = field(30)?;
    let length = field(38)?;
    let end = start
        .checked_add(length)
        .filter(|_| length != 0)
        .ok_or_else(|| malformed("PCI resource extent"))?;
    // Translation is a two's-complement host-to-bus offset. Modular addition
    // permits a negative offset, but an actual PCI interval must never wrap.
    let device_start = start.wrapping_add(translation);
    let device_end = device_start
        .checked_add(length)
        .ok_or_else(|| malformed("PCI translated resource overflow"))?;
    // EDK2 PciIoGetBarAttributes returns the BAR alignment mask in AddrRangeMax.
    // Accept only that precise power-of-two form or the specification's end.
    let alignment_form = bar
        && length.is_power_of_two()
        && maximum == length - 1
        && device_start.is_multiple_of(length);
    if maximum != end - 1 && !alignment_form {
        return Err(malformed("PCI resource maximum/length"));
    }
    match kind {
        0 if matches!(granularity, 32 | 64)
            && bytes[5] & !0x3f == 0
            && end <= width.limit()
            && (granularity == 64 || device_end <= 1 << 32) => {}
        1 if granularity == 0
            && bytes[5] & !0x33 == 0
            && end <= 1 << 16
            && device_end <= 1 << 16 => {}
        2 if !bar && granularity == 0 && bytes[5] == 0 && translation == 0 && end <= 256 => {}
        _ => return Err(malformed("PCI resource type/attributes/address space")),
    }
    Ok(Resource {
        kind,
        start,
        end,
        translation,
    })
}

/// The same bounded decoder handles firmware-owned root buffers and temporary
/// BAR pools. Each read is checked before access; no guessed allocation extent.
fn parse_resources<'a>(
    mut read: impl FnMut(usize, usize) -> Option<&'a [u8]>,
    bar: bool,
    width: PhysicalWidth,
    mut visit: impl FnMut(Resource) -> Result<(), Error>,
) -> Result<usize, Error> {
    let mut checksum = 0u8;
    for index in 0..=MAX_MMIO_RANGES {
        let offset = index * QWORD_BYTES;
        let header = read(offset, 2)
            .filter(|bytes| bytes.len() == 2)
            .ok_or_else(|| malformed("PCI resource terminator range"))?;
        if header[0] == 0x79 {
            if index == 0
                || (header[1] != 0 && checksum.wrapping_add(header[0]).wrapping_add(header[1]) != 0)
            {
                return Err(malformed("PCI resource count/checksum"));
            }
            return Ok(index);
        }
        if index == MAX_MMIO_RANGES {
            return Err(malformed("PCI resource descriptor capacity"));
        }
        let bytes = read(offset, QWORD_BYTES).ok_or_else(|| malformed("PCI QWORD extent"))?;
        let item = resource(bytes, bar, width)?;
        checksum = bytes
            .iter()
            .fold(checksum, |sum, byte| sum.wrapping_add(*byte));
        visit(item)?;
    }
    Err(malformed("PCI resource terminator"))
}

fn firmware_resources(
    map: &MemoryMap<'_>,
    pointer: *mut c_void,
    bar: bool,
    width: PhysicalWidth,
    visit: impl FnMut(Resource) -> Result<(), Error>,
) -> Result<usize, Error> {
    parse_resources(
        |offset, length| {
            let address = (pointer as usize as u64).checked_add(offset as u64)?;
            map.firmware_bytes(address, length)
        },
        bar,
        width,
        visit,
    )
}

#[derive(Clone, Copy, Default)]
struct Root {
    segment: u32,
    buses: [u64; 4],
}

#[derive(Clone, Copy, Default)]
struct Window {
    root: usize,
    resource: Resource,
}

struct Roots {
    roots: [Root; MAX_ROOTS],
    count: usize,
    windows: [Window; MAX_MMIO_RANGES],
    window_count: usize,
}

impl Roots {
    fn add(&mut self, segment: u32) -> Result<usize, Error> {
        let index = self.count;
        let root = self
            .roots
            .get_mut(index)
            .ok_or_else(|| malformed("PCI root capacity"))?;
        *root = Root {
            segment,
            ..Root::default()
        };
        self.count += 1;
        Ok(index)
    }

    fn insert(&mut self, root: usize, item: Resource) -> Result<(), Error> {
        match item.kind {
            0 => {
                if self.windows[..self.window_count]
                    .iter()
                    .any(|old| item.start < old.resource.end && old.resource.start < item.end)
                {
                    return Err(malformed("overlapping PCI root memory windows"));
                }
                let slot = self
                    .windows
                    .get_mut(self.window_count)
                    .ok_or_else(|| malformed("PCI root window capacity"))?;
                *slot = Window {
                    root,
                    resource: item,
                };
                self.window_count += 1;
            }
            2 => {
                for bus in item.start..item.end {
                    let word = bus as usize / 64;
                    let bit = 1 << (bus % 64);
                    if self.roots[..self.count].iter().any(|old| {
                        old.segment == self.roots[root].segment && old.buses[word] & bit != 0
                    }) {
                        return Err(malformed("overlapping PCI root bus ownership"));
                    }
                    self.roots[root].buses[word] |= bit;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn owner(
        &self,
        segment: usize,
        bus: usize,
        device: usize,
        function: usize,
    ) -> Result<usize, Error> {
        if segment > u32::MAX as usize || bus > 255 || device > 31 || function > 7 {
            return Err(malformed("PCI device location"));
        }
        self.roots[..self.count]
            .iter()
            .position(|root| {
                root.segment as usize == segment && root.buses[bus / 64] & (1 << (bus % 64)) != 0
            })
            .ok_or_else(|| malformed("PCI device without root bus ownership"))
    }

    fn check_bar(&self, root: usize, item: Resource) -> Result<(), Error> {
        if item.kind == 0
            && !self.windows[..self.window_count].iter().any(|window| {
                window.root == root
                    && window.resource.start <= item.start
                    && item.end <= window.resource.end
                    && window.resource.translation == item.translation
            })
        {
            return Err(malformed("PCI BAR outside owning root window"));
        }
        Ok(())
    }
}

/// Enumerates without connecting drivers, opening exclusive ownership, or
/// touching configuration registers. The temporary handle pool is always freed.
fn interfaces(
    services: *mut efi::BootServices,
    map: &MemoryMap<'_>,
    mut guid: efi::Guid,
    mut visit: impl FnMut(*mut c_void) -> Result<(), Error>,
) -> Result<usize, Error> {
    let mut count = 0;
    let mut handles = ptr::null_mut();
    // SAFETY: Boot Services remain live; GUID and distinct outputs are writable
    // locals. SUCCESS transfers an initialized handle array allocated by firmware.
    let status = unsafe {
        ((*services).locate_handle_buffer)(
            efi::BY_PROTOCOL,
            &mut guid,
            ptr::null_mut(),
            &mut count,
            &mut handles,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "LocateHandleBuffer(PCI)",
            status.as_usize(),
        ));
    }
    if handles.is_null() {
        return Err(malformed("PCI null handle array"));
    }
    let result = (|| {
        let bytes = count
            .checked_mul(mem::size_of::<efi::Handle>())
            .filter(|_| (1..=MAX_HANDLES).contains(&count))
            .ok_or_else(|| malformed("PCI handle count"))?;
        if !(handles as usize).is_multiple_of(mem::align_of::<efi::Handle>())
            || map.firmware_bytes(handles as usize as u64, bytes).is_none()
        {
            return Err(malformed("PCI handle array range/alignment"));
        }
        // SAFETY: the checked alignment/count and complete readable RAM span
        // cover the live initialized handle pool; no reference survives FreePool.
        let handles = unsafe { slice::from_raw_parts(handles, count) };
        for (index, &handle) in handles.iter().enumerate() {
            if handle.is_null() || handles[..index].contains(&handle) {
                return Err(malformed("PCI null/duplicate handle"));
            }
            let mut interface = ptr::null_mut();
            // SAFETY: the enumerated handle belongs to this protocol, Boot
            // Services are active, and the output is a writable local pointer.
            let status =
                unsafe { ((*services).handle_protocol)(handle, &mut guid, &mut interface) };
            if status.is_error() {
                return Err(Error::Firmware("HandleProtocol(PCI)", status.as_usize()));
            }
            visit(interface)?;
        }
        Ok(count)
    })();
    chainload::free_pool(services, handles.cast())?;
    result
}

fn interface_bytes<'a>(
    map: &'a MemoryMap<'_>,
    pointer: *mut c_void,
    length: usize,
) -> Result<&'a [u8], Error> {
    if !(pointer as usize).is_multiple_of(mem::align_of::<usize>()) {
        return Err(malformed("PCI protocol alignment"));
    }
    map.firmware_bytes(pointer as usize as u64, length)
        .ok_or_else(|| malformed("PCI protocol extent"))
}

fn function(map: &MemoryMap<'_>, bytes: &[u8], offset: usize) -> Result<usize, Error> {
    let target = read_u64(bytes, offset).ok_or_else(|| malformed("PCI protocol function slot"))?;
    if map.firmware_bytes(target, 1).is_none() {
        return Err(malformed("PCI protocol function range"));
    }
    Ok(target as usize)
}

fn check_bar_config(
    bytes: &[u8; 64],
    bar: usize,
    item: Resource,
    previous: Option<Resource>,
) -> Result<(), Error> {
    // A multi-descriptor BAR must describe one contiguous, ordered resource;
    // duplicate/overlapping pieces cannot inflate coverage or BAR counters.
    if let Some(previous) = previous {
        return if previous.kind == item.kind
            && previous.end == item.start
            && previous.translation == item.translation
        {
            Ok(())
        } else {
            Err(malformed("PCI BAR resource continuation"))
        };
    }
    let low = read_u32(bytes, 16 + bar * 4).ok_or_else(|| malformed("PCI BAR config offset"))?;
    let (kind, address) = if low & 1 != 0 {
        (1, u64::from(low & !3))
    } else {
        let high = if low & 6 == 4 {
            read_u32(bytes, 20 + bar * 4).ok_or_else(|| malformed("PCI BAR high config offset"))?
        } else {
            0
        };
        (0, (u64::from(high) << 32) | u64::from(low & !15))
    };
    if item.kind != kind || item.start.wrapping_add(item.translation) != address {
        return Err(malformed("PCI BAR resource/config mismatch"));
    }
    Ok(())
}

/// Reads the already assigned BARs; never writes all-ones sizing probes or
/// command/enable bits. Unsupported nonzero BARs are errors, not missing MMIO.
pub(crate) fn collect(
    system_table: *mut efi::SystemTable,
    map: &MemoryMap<'_>,
    width: PhysicalWidth,
    serial: &mut SerialPort,
) -> Result<MmioMap, Error> {
    let services = chainload::boot_services(system_table)?;
    let mut roots = Roots {
        roots: [Root::default(); MAX_ROOTS],
        count: 0,
        windows: [Window::default(); MAX_MMIO_RANGES],
        window_count: 0,
    };
    interfaces(services, map, ROOT_GUID, |interface| {
        let bytes = interface_bytes(map, interface, ROOT_SEGMENT + 4)?;
        let segment = read_u32(bytes, ROOT_SEGMENT).ok_or_else(|| malformed("PCI root segment"))?;
        let target = function(map, bytes, ROOT_CONFIGURATION)?;
        // SAFETY: HandleProtocol supplied this live aligned RootBridgeIo
        // interface. Its Configuration ABI slot at 136 and code range were
        // checked; x86-64 UEFI uses 8-byte pointers and SegmentNumber at 144.
        let configuration: Configuration = unsafe { mem::transmute(target) };
        let mut resources = ptr::null_mut();
        // SAFETY: interface is firmware-owned and resources is a writable local.
        // Configuration returns read-only firmware storage, NOT a caller pool.
        let status = unsafe { configuration(interface, &mut resources) };
        if status.is_error() {
            return Err(Error::Firmware("PCI root Configuration", status.as_usize()));
        }
        let root = roots.add(segment)?;
        firmware_resources(map, resources, false, width, |item| {
            roots.insert(root, item)
        })?;
        if roots.roots[root].buses == [0; 4] {
            return Err(malformed("PCI root has no bus range"));
        }
        Ok(())
    })?;
    let mut bars = 0usize;
    let devices = interfaces(services, map, pci_io::PROTOCOL_GUID, |interface| {
        let bytes = interface_bytes(map, interface, mem::size_of::<pci_io::Protocol>())?;
        let location = function(map, bytes, mem::offset_of!(pci_io::Protocol, get_location))?;
        let config = function(map, bytes, mem::offset_of!(pci_io::Protocol, pci))?;
        let get_bar = function(
            map,
            bytes,
            mem::offset_of!(pci_io::Protocol, get_bar_attributes),
        )?;
        // SAFETY: the full aligned PCI I/O interface and each non-null code
        // address were checked. These r-efi aliases match the UEFI function
        // slots; only read/Get operations are invoked before ExitBootServices.
        let (location, config, get_bar): (
            pci_io::ProtocolGetLocation,
            pci_io::ProtocolConfig,
            pci_io::ProtocolGetBarAttributes,
        ) = unsafe {
            (
                mem::transmute(location),
                mem::transmute(config),
                mem::transmute(get_bar),
            )
        };
        let (mut segment, mut bus, mut device, mut function) = (0, 0, 0, 0);
        // SAFETY: each output names a distinct initialized usize local; This
        // remains the live firmware PCI I/O instance for the entire call.
        let status = unsafe {
            location(
                interface.cast(),
                &mut segment,
                &mut bus,
                &mut device,
                &mut function,
            )
        };
        if status.is_error() {
            return Err(Error::Firmware("PCI GetLocation", status.as_usize()));
        }
        let owner = roots.owner(segment, bus, device, function)?;
        let mut header = [0u8; 64];
        // SAFETY: UINT8/count64 accesses exactly the standard PCI config header
        // and the complete writable output array. This is a read, never sizing.
        let status = unsafe {
            config(
                interface.cast(),
                pci_io::WIDTH_UINT8,
                0,
                header.len(),
                header.as_mut_ptr().cast(),
            )
        };
        if status.is_error() {
            return Err(Error::Firmware("PCI config header read", status.as_usize()));
        }
        if header[..2] == [0xff, 0xff] {
            return Err(malformed("PCI device disappeared"));
        }
        let count = match header[14] & 0x7f {
            0 => 6,
            1 => 2,
            _ => return Err(malformed("unsupported PCI header layout")),
        };
        let mut bar = 0;
        while bar < count {
            let low = read_u32(&header, 16 + bar * 4).ok_or_else(|| malformed("PCI BAR config"))?;
            let wide = low & 1 == 0 && low & 6 == 4;
            if (wide && bar + 1 == count) || (low & 1 == 0 && low & 6 == 6) {
                return Err(malformed("PCI BAR encoding"));
            }
            let mut resources = ptr::null_mut();
            // SAFETY: BAR index is within this header's implemented slot count;
            // optional Supports is null, Resources is writable and receives a
            // caller-owned AllocatePool buffer only on SUCCESS.
            let status =
                unsafe { get_bar(interface.cast(), bar as u8, ptr::null_mut(), &mut resources) };
            if status == efi::Status::UNSUPPORTED && low == 0 {
                bar += 1;
                continue;
            }
            if status.is_error() {
                return Err(Error::Firmware("PCI GetBarAttributes", status.as_usize()));
            }
            if resources.is_null() {
                return Err(malformed("PCI null BAR resources"));
            }
            let mut previous = None;
            let result = firmware_resources(map, resources, true, width, |item| {
                check_bar_config(&header, bar, item, previous)?;
                roots.check_bar(owner, item)?;
                previous = Some(item);
                Ok(())
            });
            chainload::free_pool(services, resources)?;
            result?;
            if low & 1 == 0 {
                bars += 1;
            }
            bar += if wide { 2 } else { 1 };
        }
        Ok(())
    })?;
    let mut output = MmioMap::empty(width)?;
    for window in &roots.windows[..roots.window_count] {
        output.insert(window.resource.page_range(width)?, width)?;
    }
    let _ = writeln!(
        serial,
        "thin-hv: preflight PCI MMIO roots={} devices={devices} windows={} bars={bars} ranges={} mmio_complete=0 direct_vmx_ready=0",
        roots.count,
        roots.window_count,
        output.ranges().len()
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width() -> PhysicalWidth {
        PhysicalWidth::new(48).unwrap()
    }

    fn descriptor(kind: u8, start: u64, length: u64) -> [u8; QWORD_BYTES] {
        let mut bytes = [0; QWORD_BYTES];
        bytes[..4].copy_from_slice(&[0x8a, 0x2b, 0, kind]);
        bytes[6..14].copy_from_slice(&(if kind == 0 { 64u64 } else { 0 }).to_le_bytes());
        bytes[14..22].copy_from_slice(&start.to_le_bytes());
        bytes[22..30].copy_from_slice(&(start + length - 1).to_le_bytes());
        bytes[38..46].copy_from_slice(&length.to_le_bytes());
        bytes
    }

    fn roots() -> Roots {
        Roots {
            roots: [Root::default(); MAX_ROOTS],
            count: 0,
            windows: [Window::default(); MAX_MMIO_RANGES],
            window_count: 0,
        }
    }

    #[test]
    fn preflight_pci_protocol_abi_and_descriptor_extents() {
        assert_eq!(ROOT_CONFIGURATION, 17 * mem::size_of::<usize>());
        assert_eq!(ROOT_SEGMENT, ROOT_CONFIGURATION + mem::size_of::<usize>());
        assert_eq!(mem::offset_of!(pci_io::Protocol, pci), 48);
        assert_eq!(mem::offset_of!(pci_io::Protocol, get_location), 112);
        assert_eq!(mem::offset_of!(pci_io::Protocol, get_bar_attributes), 128);
        assert_eq!(mem::size_of::<pci_io::Protocol>(), 160);
        let high = resource(&descriptor(0, 56 << 40, 8 << 40), false, width()).unwrap();
        assert_eq!(high.page_range(width()).unwrap().end(), 64 << 40);
        let last = resource(&descriptor(0, (1 << 48) - PAGE, PAGE), false, width()).unwrap();
        assert_eq!(last.page_range(width()).unwrap().end(), 1 << 48);
        assert!(last.page_range(PhysicalWidth::new(36).unwrap()).is_err());
        let unaligned = resource(&descriptor(0, 0x8000_0080, 128), true, width()).unwrap();
        let page = unaligned.page_range(width()).unwrap();
        assert_eq!((page.start(), page.end()), (0x8000_0000, 0x8000_1000));
    }

    #[test]
    fn preflight_pci_translation_and_edk2_alignment_are_not_host_addresses() {
        let mut bytes = descriptor(0, 1 << 40, PAGE);
        bytes[6..14].copy_from_slice(&32u64.to_le_bytes());
        bytes[30..38].copy_from_slice(&(0x8000_0000u64.wrapping_sub(1 << 40)).to_le_bytes());
        let item = resource(&bytes, true, width()).unwrap();
        assert_eq!(item.start, 1 << 40);
        assert_eq!(item.start.wrapping_add(item.translation), 0x8000_0000);
        bytes[22..30].copy_from_slice(&(PAGE - 1).to_le_bytes());
        assert!(resource(&bytes, true, width()).is_ok());
        assert!(resource(&bytes, false, width()).is_err());
        bytes[22..30].copy_from_slice(&(2 * PAGE - 1).to_le_bytes());
        assert!(resource(&bytes, true, width()).is_err());
        bytes[22..30].copy_from_slice(&(PAGE - 1).to_le_bytes());
        bytes[30..38].copy_from_slice(&((1u64 << 32).wrapping_sub(1 << 40)).to_le_bytes());
        assert!(resource(&bytes, true, width()).is_err());
    }

    #[test]
    fn preflight_pci_rejects_malformed_resource_fields_and_arithmetic() {
        let good = descriptor(0, 0x8000_0000, PAGE);
        for (offset, value) in [(0, 0x87), (1, 0x2a), (2, 1), (3, 3), (4, 0x80), (5, 0x80)] {
            let mut bad = good;
            bad[offset] = value;
            assert!(resource(&bad, false, width()).is_err(), "offset={offset}");
        }
        for (offset, value) in [
            (6, 0),
            (14, u64::MAX - PAGE + 1),
            (22, 0),
            (38, 0),
            (38, u64::MAX),
        ] {
            let mut bad = good;
            bad[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            assert!(resource(&bad, false, width()).is_err(), "offset={offset}");
        }
        assert!(resource(&good[..45], false, width()).is_err());
        assert!(resource(&descriptor(0, 1 << 48, PAGE), false, width()).is_err());
        assert!(resource(&descriptor(1, 65535, 2), false, width()).is_err());
        assert!(resource(&descriptor(2, 255, 2), false, width()).is_err());
        assert!(resource(&descriptor(2, 0, 256), true, width()).is_err());
        assert!(resource(&descriptor(2, 0, 256), false, width()).is_ok());
    }

    #[test]
    fn preflight_pci_resource_walk_requires_bounded_complete_terminated_checksum() {
        let decode = |bytes: &[u8]| {
            parse_resources(
                |offset, length| bytes.get(offset..offset + length),
                false,
                width(),
                |_| Ok(()),
            )
        };
        let mut bytes = std::vec::Vec::from(descriptor(0, 0x8000_0000, PAGE));
        bytes.extend_from_slice(&[0x79, 0]);
        assert_eq!(decode(&bytes).unwrap(), 1);
        for size in 0..bytes.len() {
            assert!(decode(&bytes[..size]).is_err());
        }
        let checksum = bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        bytes[47] = 0u8.wrapping_sub(checksum);
        assert!(decode(&bytes).is_ok());
        bytes[47] = if bytes[47] == 1 { 2 } else { 1 };
        assert!(decode(&bytes).is_err());
        assert!(decode(&[0x79, 0]).is_err());
        let mut many = descriptor(0, 0x8000_0000, PAGE).repeat(MAX_MMIO_RANGES);
        many.extend_from_slice(&[0x79, 0]);
        assert_eq!(decode(&many).unwrap(), MAX_MMIO_RANGES);
        let mut excess = descriptor(0, 0x8000_0000, PAGE).repeat(MAX_MMIO_RANGES + 1);
        excess.extend_from_slice(&[0x79, 0]);
        assert!(decode(&excess).is_err());
    }

    #[test]
    fn preflight_pci_root_bus_window_ownership_and_capacity() {
        let mut roots = roots();
        let first = roots.add(0).unwrap();
        let second = roots.add(1).unwrap();
        let buses = resource(&descriptor(2, 0, 256), false, width()).unwrap();
        roots.insert(first, buses).unwrap();
        roots.insert(second, buses).unwrap();
        assert_eq!(roots.owner(0, 255, 31, 7).unwrap(), first);
        assert_eq!(roots.owner(1, 0, 0, 0).unwrap(), second);
        for location in [(2, 0, 0, 0), (0, 256, 0, 0), (0, 0, 32, 0), (0, 0, 0, 8)] {
            assert!(
                roots
                    .owner(location.0, location.1, location.2, location.3)
                    .is_err()
            );
        }
        assert!(roots.insert(first, buses).is_err());
        let window = resource(&descriptor(0, 0x8000_0000, 1 << 28), false, width()).unwrap();
        roots.insert(first, window).unwrap();
        assert!(roots.insert(second, window).is_err());
        let bar = resource(&descriptor(0, 0x8000_0000, PAGE), true, width()).unwrap();
        assert!(roots.check_bar(first, bar).is_ok());
        assert!(roots.check_bar(second, bar).is_err());
        assert!(
            roots
                .check_bar(
                    first,
                    Resource {
                        translation: PAGE,
                        ..bar
                    }
                )
                .is_err()
        );
        assert!(
            roots
                .check_bar(
                    first,
                    Resource {
                        end: window.end + PAGE,
                        ..bar
                    }
                )
                .is_err()
        );
        for _ in 2..MAX_ROOTS {
            roots.add(2).unwrap();
        }
        assert!(roots.add(2).is_err());
        for index in 1..MAX_MMIO_RANGES {
            roots
                .insert(
                    first,
                    Resource {
                        start: (1 << 40) + index as u64 * PAGE,
                        end: (1 << 40) + (index as u64 + 1) * PAGE,
                        ..bar
                    },
                )
                .unwrap();
        }
        assert!(
            roots
                .insert(
                    first,
                    Resource {
                        start: 2 << 40,
                        end: (2 << 40) + PAGE,
                        ..bar
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn preflight_pci_bar_config_cross_check_uses_device_not_host_address() {
        let mut header = [0u8; 64];
        let mut item = resource(&descriptor(0, 1 << 40, PAGE), true, width()).unwrap();
        header[16..20].copy_from_slice(&4u32.to_le_bytes());
        header[20..24].copy_from_slice(&256u32.to_le_bytes());
        check_bar_config(&header, 0, item, None).unwrap();
        header[16..20].copy_from_slice(&0x8000_0000u32.to_le_bytes());
        assert!(check_bar_config(&header, 0, item, None).is_err());
        item.translation = 0x8000_0000u64.wrapping_sub(1 << 40);
        check_bar_config(&header, 0, item, None).unwrap();
        let next = Resource {
            start: item.end,
            end: item.end + PAGE,
            ..item
        };
        check_bar_config(&header, 0, next, Some(item)).unwrap();
        assert!(check_bar_config(&header, 0, item, Some(item)).is_err());
        assert!(
            check_bar_config(
                &header,
                0,
                Resource {
                    translation: 0,
                    ..next
                },
                Some(item)
            )
            .is_err()
        );
        header[16..20].copy_from_slice(&0x8000_0001u32.to_le_bytes());
        assert!(check_bar_config(&header, 0, item, None).is_err());
        let io = resource(&descriptor(1, 0xc000, 32), true, width()).unwrap();
        header[16..20].copy_from_slice(&0xc001u32.to_le_bytes());
        check_bar_config(&header, 0, io, None).unwrap();
    }
}
