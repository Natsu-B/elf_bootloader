//! Read-only static SystemMemory resources through firmware's PI ACPI SDT parser.
//!
//! No AML interpreter, method execution, namespace mutation or device access.
//! Only literal regions in static namespace containers are accepted. Method and
//! conditional bodies are not executed or claimed as complete resource coverage.

use crate::SerialPort;
use crate::chainload;
use crate::chainload::Error;
use crate::platform_resources::MmioMap;
use crate::platform_snapshot::MemoryMap;
use crate::platform_snapshot::malformed;
use crate::platform_snapshot::read_u32;
use crate::platform_snapshot::read_u64;
use core::ffi::c_void;
use core::fmt::Write;
use core::mem;
use core::ptr;
use r_efi::efi;
use x86_64_hal::platform_memory::PhysicalRange;
use x86_64_hal::platform_memory::PhysicalWidth;

type Handle = *mut c_void;
type GetTable =
    unsafe extern "efiapi" fn(usize, *mut *const u8, *mut u32, *mut usize) -> efi::Status;
type OpenSdt = unsafe extern "efiapi" fn(usize, *mut Handle) -> efi::Status;
type Close = unsafe extern "efiapi" fn(Handle) -> efi::Status;
type GetChild = unsafe extern "efiapi" fn(Handle, *mut Handle) -> efi::Status;
type GetOption =
    unsafe extern "efiapi" fn(Handle, usize, *mut u32, *mut *const u8, *mut usize) -> efi::Status;

const MAX_TABLES: usize = 4096;
const MAX_OBJECTS: usize = 65536;
const MAX_DEPTH: usize = 64;
const HEADER: usize = 36;

struct Sdt {
    get_table: GetTable,
    open_sdt: OpenSdt,
    close: Close,
    get_child: GetChild,
    get_option: GetOption,
}

fn check(status: efi::Status, context: &'static str) -> Result<(), Error> {
    if status.is_error() {
        Err(Error::Firmware(context, status.as_usize()))
    } else {
        Ok(())
    }
}

impl Sdt {
    fn locate(system: *mut efi::SystemTable, map: &MemoryMap<'_>) -> Result<Option<Self>, Error> {
        let services = chainload::boot_services(system)?;
        let mut guid = efi::Guid::from_fields(
            0xeb97088e,
            0xcfdf,
            0x49c6,
            0xbe,
            0x4b,
            &[0xd9, 0x06, 0xa5, 0xb2, 0x0e, 0x86],
        );
        let mut interface = ptr::null_mut();
        // SAFETY: Boot Services are live, with distinct writable GUID/output
        // locals. LocateProtocol neither executes AML nor changes ACPI tables.
        let status =
            unsafe { ((*services).locate_protocol)(&mut guid, ptr::null_mut(), &mut interface) };
        if status == efi::Status::NOT_FOUND {
            return Ok(None);
        }
        check(status, "LocateProtocol(ACPI SDT)")?;
        // PI x64 ABI: u32 version, four padding bytes, nine pointer-sized slots.
        let bytes = map
            .firmware_bytes(interface as usize as u64, 80)
            .ok_or_else(|| malformed("ACPI SDT protocol range"))?;
        let mut targets = [0_usize; 5];
        for (target, offset) in targets.iter_mut().zip([8, 32, 40, 48, 56]) {
            let address = read_u64(bytes, offset).ok_or_else(|| malformed("ACPI SDT ABI slot"))?;
            if map.firmware_bytes(address, 1).is_none() {
                return Err(malformed("ACPI SDT entry point range"));
            }
            *target = address as usize;
        }
        // SAFETY: the trusted firmware supplied the standard PI protocol. Each
        // non-null x64 ABI slot is in identity-readable firmware RAM. These exact
        // EFIAPI signatures have no This parameter. All calls and handle cleanup
        // finish before ExitBootServices; mutating SDT entry points are not kept.
        Ok(Some(unsafe {
            Self {
                get_table: mem::transmute::<usize, GetTable>(targets[0]),
                open_sdt: mem::transmute::<usize, OpenSdt>(targets[1]),
                close: mem::transmute::<usize, Close>(targets[2]),
                get_child: mem::transmute::<usize, GetChild>(targets[3]),
                get_option: mem::transmute::<usize, GetOption>(targets[4]),
            }
        }))
    }

    fn close(&self, handle: Handle) -> Result<(), Error> {
        // SAFETY: callers transfer one live, uniquely owned handle from this
        // protocol exactly once. No SetOption was issued, so Close only frees
        // parser storage and does not update a table or its checksum.
        check(unsafe { (self.close)(handle) }, "ACPI SDT Close")
    }

    fn option<'a>(
        &self,
        handle: Handle,
        index: usize,
        kind: u32,
        table: &'a [u8],
    ) -> Result<&'a [u8], Error> {
        let (mut actual, mut data, mut size) = (0, ptr::null(), 0);
        // SAFETY: handle remains live throughout this read-only call. All output
        // parameters are distinct initialized locals with the PI ABI types.
        check(
            unsafe { (self.get_option)(handle, index, &mut actual, &mut data, &mut size) },
            "ACPI SDT GetOption",
        )?;
        if actual == 2 && kind == 6 {
            return Err(Error::Firmware(
                "ACPI named static SystemMemory operand",
                efi::Status::UNSUPPORTED.as_usize(),
            ));
        }
        if actual != kind {
            return Err(malformed("ACPI SDT option type"));
        }
        // Use a checked offset into the validated DSDT/SSDT, never dereference an
        // arbitrary protocol-returned pointer or confidential table payload.
        let offset = (data as usize)
            .checked_sub(table.as_ptr() as usize)
            .filter(|offset| *offset >= HEADER)
            .ok_or_else(|| malformed("ACPI SDT option start"))?;
        table
            .get(
                offset
                    ..offset
                        .checked_add(size)
                        .ok_or_else(|| malformed("ACPI SDT option overflow"))?,
            )
            .filter(|value| !value.is_empty())
            .ok_or_else(|| malformed("ACPI SDT option extent"))
    }

    fn children(
        &self,
        parent: Handle,
        table: &[u8],
        depth: usize,
        inventory: &mut Inventory<'_, '_>,
    ) -> Result<(), Error> {
        if depth == MAX_DEPTH {
            return Err(malformed("ACPI SDT namespace depth"));
        }
        let mut previous = ptr::null_mut();
        loop {
            let mut next = previous;
            // SAFETY: parent and previous are live handles from this parser;
            // null previous starts enumeration. GetChild borrows previous and
            // returns a separately owned next handle (or null at the end).
            let status = unsafe { (self.get_child)(parent, &mut next) };
            let same = !previous.is_null() && previous == next;
            let closed = if previous.is_null() {
                Ok(())
            } else {
                self.close(previous)
            };
            if status.is_error() {
                closed?;
                return check(status, "ACPI SDT GetChild");
            }
            if same {
                closed?;
                return Err(malformed("ACPI SDT reused child handle"));
            }
            if let Err(error) = closed {
                if !next.is_null() {
                    self.close(next)?;
                }
                return Err(error);
            }
            if next.is_null() {
                return Ok(());
            }
            let result = (|| {
                inventory.objects += 1;
                if inventory.objects > MAX_OBJECTS {
                    return Err(malformed("ACPI SDT object count"));
                }
                match self.option(next, 0, 1, table)? {
                    [0x5b, 0x80] => {
                        let [space] = self.option(next, 2, 4, table)? else {
                            return Err(malformed("ACPI OperationRegion space width"));
                        };
                        if *space == 0 {
                            let base = literal(self.option(next, 3, 6, table)?)?;
                            let length = literal(self.option(next, 4, 6, table)?)?;
                            inventory.region(base, length)?;
                        }
                    }
                    // Scope, Device, Processor, PowerResource, ThermalZone.
                    [0x10] | [0x5b, 0x82..=0x85] => {
                        self.children(next, table, depth + 1, inventory)?
                    }
                    // Methods/conditionals require evaluation. Do not scan byte
                    // patterns in them or mistake Buffer contents for opcodes.
                    [0x14] | [0xa0..=0xa2] => inventory.dynamic_bodies += 1,
                    _ => {}
                }
                Ok(())
            })();
            if let Err(error) = result {
                self.close(next)?;
                return Err(error);
            }
            previous = next;
        }
    }
}

/// Only complete AML integer literals, not names, expressions or method calls.
fn literal(bytes: &[u8]) -> Result<u64, Error> {
    match bytes {
        [0] => Ok(0),
        [1] => Ok(1),
        [0xff] => Ok(u64::MAX),
        [0x0a, value] => Ok(u64::from(*value)),
        [0x0b, a, b] => Ok(u64::from(u16::from_le_bytes([*a, *b]))),
        [0x0c, a, b, c, d] => Ok(u64::from(u32::from_le_bytes([*a, *b, *c, *d]))),
        [0x0e, a, b, c, d, e, f, g, h] => Ok(u64::from_le_bytes([*a, *b, *c, *d, *e, *f, *g, *h])),
        _ => Err(Error::Firmware(
            "ACPI nonliteral static SystemMemory region",
            efi::Status::UNSUPPORTED.as_usize(),
        )),
    }
}

fn region_range(base: u64, length: u64, width: PhysicalWidth) -> Result<PhysicalRange, Error> {
    let end = base
        .checked_add(length)
        .filter(|end| length != 0 && *end <= width.limit())
        .and_then(|end| end.checked_add(4095))
        .map(|end| end & !4095)
        .ok_or_else(|| malformed("ACPI SystemMemory extent"))?;
    PhysicalRange::new(base & !4095, end, width)
        .map_err(|_| malformed("ACPI SystemMemory physical width"))
}

struct Inventory<'a, 'm> {
    map: &'a MemoryMap<'m>,
    width: PhysicalWidth,
    output: MmioMap,
    objects: usize,
    regions: usize,
    dynamic_bodies: usize,
}

impl Inventory<'_, '_> {
    fn region(&mut self, base: u64, length: u64) -> Result<(), Error> {
        let range = region_range(base, length, self.width)?;
        let mut cursor = range.start();
        // Preserve existing RAM/NVS/runtime attributes. Only absent/reserved
        // pages or existing MMIO become UC overrides, checked again by GCD/EPT.
        while cursor < range.end() {
            let mut end = range.end();
            let mut kind = efi::RESERVED_MEMORY_TYPE;
            for index in 0..self.map.count() {
                let region = self
                    .map
                    .region(index)
                    .ok_or_else(|| malformed("AML memory descriptor"))?;
                let limit = region
                    .end()
                    .ok_or_else(|| malformed("AML memory descriptor end"))?;
                if region.start <= cursor && cursor < limit {
                    kind = region.kind;
                    end = end.min(limit);
                } else if region.start > cursor {
                    end = end.min(region.start);
                }
            }
            match kind {
                0 | 11 => self.output.insert(
                    PhysicalRange::new(cursor, end, self.width)
                        .map_err(|_| malformed("AML MMIO split"))?,
                    self.width,
                )?,
                1..=7 | 9 | 10 | 14 => {}
                _ => return Err(malformed("ACPI SystemMemory unavailable backing")),
            }
            cursor = end;
        }
        self.regions += 1;
        Ok(())
    }
}

/// Collects additional static namespace resources without changing firmware.
/// Missing protocol is explicit unsupported discovery, never a QEMU fallback.
pub(crate) fn collect(
    system: *mut efi::SystemTable,
    map: &MemoryMap<'_>,
    width: PhysicalWidth,
    serial: &mut SerialPort,
) -> Result<MmioMap, Error> {
    let sdt = Sdt::locate(system, map)?.ok_or(Error::Firmware(
        "ACPI SDT unavailable",
        efi::Status::UNSUPPORTED.as_usize(),
    ))?;
    let mut inventory = Inventory {
        map,
        width,
        output: MmioMap::empty(width)?,
        objects: 0,
        regions: 0,
        dynamic_bodies: 0,
    };
    let mut aml_tables = 0;
    for index in 0..=MAX_TABLES {
        let (mut pointer, mut version, mut key) = (ptr::null(), 0, 0);
        // SAFETY: the live protocol receives distinct ABI-correct output locals.
        // GetAcpiTable returns a borrowed installed table, not an owned buffer.
        let status = unsafe { (sdt.get_table)(index, &mut pointer, &mut version, &mut key) };
        if status == efi::Status::NOT_FOUND {
            if aml_tables == 0 {
                return Err(malformed("ACPI SDT has no AML tables"));
            }
            let _ = writeln!(
                serial,
                "thin-hv: preflight AML MMIO source=firmware-sdt tables={aml_tables} static_regions={} mmio_ranges={} unevaluated_bodies={} aml_executed=0 mmio_complete=0",
                inventory.regions,
                inventory.output.ranges().len(),
                inventory.dynamic_bodies
            );
            return Ok(inventory.output);
        }
        check(status, "ACPI SDT GetAcpiTable")?;
        if index == MAX_TABLES {
            return Err(malformed("ACPI SDT table capacity"));
        }
        let header = map
            .firmware_bytes(pointer as usize as u64, HEADER)
            .ok_or_else(|| malformed("ACPI SDT header range"))?;
        if !matches!(&header[..4], b"DSDT" | b"SSDT") {
            continue;
        }
        let length = read_u32(header, 4).ok_or_else(|| malformed("AML table length"))? as usize;
        if !(HEADER..=1024 * 1024).contains(&length) {
            return Err(malformed("AML table size bound"));
        }
        let table = map
            .firmware_bytes(pointer as usize as u64, length)
            .ok_or_else(|| malformed("AML table range"))?;
        if table.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
            return Err(malformed("AML table checksum"));
        }
        let mut root = ptr::null_mut();
        // SAFETY: key came from this protocol's installed DSDT/SSDT. Its full
        // immutable table passed RAM, length and checksum checks. OpenSdt creates
        // parser storage only; the root is closed on every traversal result.
        check(
            unsafe { (sdt.open_sdt)(key, &mut root) },
            "ACPI SDT OpenSdt",
        )?;
        if root.is_null() {
            return Err(malformed("ACPI SDT null root"));
        }
        let result = sdt.children(root, table, 0, &mut inventory);
        sdt.close(root)?;
        result?;
        aml_tables += 1;
    }
    Err(malformed("ACPI SDT table capacity"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(base: u64, pages: u64, kind: u32) -> [u8; 48] {
        let mut bytes = [0; 48];
        bytes[..4].copy_from_slice(&kind.to_le_bytes());
        bytes[8..16].copy_from_slice(&base.to_le_bytes());
        bytes[24..32].copy_from_slice(&pages.to_le_bytes());
        bytes[32..40].copy_from_slice(&efi::MEMORY_WB.to_le_bytes());
        bytes
    }

    fn inventory<'a, 'm>(map: &'a MemoryMap<'m>) -> Inventory<'a, 'm> {
        let width = PhysicalWidth::new(48).unwrap();
        Inventory {
            map,
            width,
            output: MmioMap::empty(width).unwrap(),
            objects: 0,
            regions: 0,
            dynamic_bodies: 0,
        }
    }

    struct TestNode<'a> {
        table: &'a [u8],
        options: [(u32, usize, usize); 5],
        children: std::vec::Vec<Handle>,
        closed: core::cell::Cell<usize>,
    }

    // Test callbacks model PI ownership, including keeping the previous child
    // live while asking for its successor. Test storage itself lives to test end.
    unsafe extern "efiapi" fn test_child(parent: Handle, child: *mut Handle) -> efi::Status {
        // SAFETY: this test only passes aligned, live TestNode pointers and a
        // distinct writable Handle local; Vec contents are stable during calls.
        let (parent, previous) = unsafe { (&*parent.cast::<TestNode<'_>>(), *child) };
        assert_eq!(parent.closed.get(), 0);
        let index = if previous.is_null() {
            0
        } else {
            // SAFETY: every child is a TestNode retained by the test until all
            // calls finish, even after its logical parser handle is closed.
            assert_eq!(unsafe { &*previous.cast::<TestNode<'_>>() }.closed.get(), 0);
            parent
                .children
                .iter()
                .position(|&handle| handle == previous)
                .unwrap()
                + 1
        };
        // SAFETY: child is the exclusive writable output local supplied above.
        unsafe {
            *child = parent
                .children
                .get(index)
                .copied()
                .unwrap_or(ptr::null_mut())
        };
        efi::Status::SUCCESS
    }

    unsafe extern "efiapi" fn test_close(handle: Handle) -> efi::Status {
        // SAFETY: handle points to a live test-owned TestNode, never freed here.
        let node = unsafe { &*handle.cast::<TestNode<'_>>() };
        assert_eq!(node.closed.replace(node.closed.get() + 1), 0);
        efi::Status::SUCCESS
    }

    unsafe extern "efiapi" fn test_option(
        handle: Handle,
        index: usize,
        kind: *mut u32,
        data: *mut *const u8,
        size: *mut usize,
    ) -> efi::Status {
        // SAFETY: handle is a live TestNode; the three distinct output locals
        // have matching ABI types. Malformed addresses are returned as numbers,
        // never dereferenced, to test the production extent checks.
        unsafe {
            let node = &*handle.cast::<TestNode<'_>>();
            assert_eq!(node.closed.get(), 0);
            let (actual, offset, length) = node.options[index];
            *kind = actual;
            *data = node.table.as_ptr().wrapping_add(offset);
            *size = length;
        }
        efi::Status::SUCCESS
    }

    unsafe extern "efiapi" fn unused_table(
        _: usize,
        _: *mut *const u8,
        _: *mut u32,
        _: *mut usize,
    ) -> efi::Status {
        efi::Status::ABORTED
    }
    unsafe extern "efiapi" fn unused_open(_: usize, _: *mut Handle) -> efi::Status {
        efi::Status::ABORTED
    }

    #[test]
    fn firmware_sdt_child_operands_and_error_cleanup_follow_the_pi_contract() {
        let map_bytes = descriptor(0x1000, 1, 7);
        let map = MemoryMap::test_snapshot(&map_bytes).unwrap();
        let mut table = std::vec![0; HEADER];
        // OpRegion, space, base DWORD literal, length BYTE literal, Method.
        table.extend_from_slice(&[0x5b, 0x80, 0, 0x0c, 1, 0x51, 0x34, 0x12, 0x0a, 0x5a, 0x14]);
        let sdt = Sdt {
            get_table: unused_table,
            open_sdt: unused_open,
            close: test_close,
            get_child: test_child,
            get_option: test_option,
        };
        // CHILD is 6, not OP (3); NAME_STRING requires unsupported resolution.
        for (operand, valid) in [
            ((6, HEADER + 3, 5), true),
            ((3, HEADER + 3, 5), false),
            ((2, HEADER + 3, 5), false),
            ((6, 0, 5), false),
            ((6, table.len(), 5), false),
            ((6, HEADER + 3, usize::MAX), false),
        ] {
            let mut region = TestNode {
                table: &table,
                options: [
                    (1, HEADER, 2),
                    (2, HEADER, 1),
                    (4, HEADER + 2, 1),
                    operand,
                    (6, HEADER + 8, 2),
                ],
                children: std::vec![],
                closed: core::cell::Cell::new(0),
            };
            let mut method = TestNode {
                table: &table,
                options: [(1, HEADER + 10, 1); 5],
                children: std::vec![],
                closed: core::cell::Cell::new(0),
            };
            let mut root = TestNode {
                table: &table,
                options: [(0, 0, 0); 5],
                children: std::vec![
                    ptr::from_mut(&mut region).cast(),
                    ptr::from_mut(&mut method).cast()
                ],
                closed: core::cell::Cell::new(0),
            };
            let mut inv = inventory(&map);
            let result = sdt.children(ptr::from_mut(&mut root).cast(), &table, 0, &mut inv);
            assert_eq!(result.is_ok(), valid);
            assert_eq!(region.closed.get(), 1);
            assert_eq!(method.closed.get(), usize::from(valid));
            assert_eq!(root.closed.get(), 0); // Caller, not walker, owns root.
            sdt.close(ptr::from_mut(&mut root).cast()).unwrap();
            if valid {
                assert_eq!(inv.dynamic_bodies, 1);
                assert_eq!(
                    inv.output.ranges(),
                    &[PhysicalRange::new(0x12345000, 0x12346000, inv.width).unwrap()]
                );
            }
        }
        let mut root = TestNode {
            table: &table,
            options: [(0, 0, 0); 5],
            children: std::vec![],
            closed: core::cell::Cell::new(0),
        };
        assert!(
            sdt.children(
                ptr::from_mut(&mut root).cast(),
                &table,
                MAX_DEPTH,
                &mut inventory(&map)
            )
            .is_err()
        );
        sdt.close(ptr::from_mut(&mut root).cast()).unwrap();
    }

    #[test]
    fn aml_regions_preserve_ram_and_runtime_while_splitting_holes() {
        for kind in [1, 2, 3, 4, 5, 6, 7, 9, 10, 14] {
            // Deliberately unsorted; a region spans RAM, a hole, and NVS.
            let bytes = [descriptor(0x4000, 1, 10), descriptor(0x1000, 2, kind)].concat();
            let map = MemoryMap::test_snapshot(&bytes).unwrap();
            let mut inv = inventory(&map);
            inv.region(0x1001, 0x3ffe).unwrap();
            assert_eq!(
                inv.output.ranges(),
                &[PhysicalRange::new(0x3000, 0x4000, inv.width).unwrap()]
            );
            inv.region(0x1001, 1).unwrap();
            assert_eq!(inv.output.ranges().len(), 1);
            assert_eq!(map.bytes(), bytes); // Firmware descriptors never change.
        }
        for kind in [0, 11] {
            let bytes = descriptor(0x2000, 1, kind);
            let map = MemoryMap::test_snapshot(&bytes).unwrap();
            let mut inv = inventory(&map);
            inv.region(0x1fff, 0x1002).unwrap();
            assert_eq!(
                inv.output.ranges(),
                &[PhysicalRange::new(0x1000, 0x4000, inv.width).unwrap()]
            );
        }
        for kind in [8, 12, 13, 15] {
            let bytes = descriptor(0x2000, 1, kind);
            let map = MemoryMap::test_snapshot(&bytes).unwrap();
            assert!(inventory(&map).region(0x2001, 1).is_err());
        }
    }

    #[test]
    fn static_aml_literals_and_page_extents_are_checked() {
        let width = PhysicalWidth::new(48).unwrap();
        for (bytes, value) in [
            (&[0][..], 0),
            (&[1], 1),
            (&[0xff], u64::MAX),
            (&[0x0a, 0x55], 0x55),
            (&[0x0b, 0x34, 0x12], 0x1234),
            (&[0x0c, 0, 0, 0, 0x80], 1 << 31),
            (&[0x0e, 0, 0, 0, 0, 0, 0x80, 0, 0], 1 << 47),
        ] {
            assert_eq!(literal(bytes).unwrap(), value);
            assert!(literal(&bytes[..bytes.len() - 1]).is_err());
            let mut trailing = bytes.to_vec();
            trailing.push(0);
            assert!(literal(&trailing).is_err());
        }
        for bytes in [&b"ADDR"[..], &[0x72, 0, 1, 0], &[0x68]] {
            assert!(literal(bytes).is_err());
        }
        let high = (1 << 39) + 0x101;
        let range = region_range(high, 0x5a, width).unwrap();
        assert_eq!(range.start(), 1 << 39);
        assert_eq!(range.end(), (1 << 39) + 4096);
        assert_eq!(
            region_range(width.limit() - 1, 1, width).unwrap().end(),
            width.limit()
        );
        for (base, length) in [
            (high, 0),
            (u64::MAX, 2),
            (width.limit(), 1),
            (width.limit() - 1, 2),
        ] {
            assert!(region_range(base, length, width).is_err());
        }
    }
}
