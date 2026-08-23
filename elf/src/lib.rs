//! No-allocation ELF executable parsing and image materialization.
//!
//! The ELF32 ARM path is intentionally byte-oriented: callers may pass an
//! arbitrary subslice and no header or program-header alignment is required.

#![no_std]

use core::cmp::min;

use typestate_macro::RawReg;

const ELF_IDENT_LEN: usize = 16;
const ELF32_HEADER_LEN: usize = 52;
const ELF32_PROGRAM_HEADER_LEN: usize = 32;
const ELF64_HEADER_LEN: usize = 64;
const ELF64_PROGRAM_HEADER_LEN: usize = 56;

const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ELFDATA2MSB: u8 = 2;
const EV_CURRENT: u32 = 1;
const ET_EXEC: u16 = 2;
const EM_ARM: u16 = 40;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const PT_NULL: u32 = 0;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;
const PT_PHDR: u32 = 6;

/// ELF word size class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfClass {
    /// ELF32 executable.
    Elf32,
    /// ELF64 executable.
    Elf64,
}

/// ELF machine type supported by this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfMachine {
    /// 32-bit ARM.
    Arm,
    /// AArch64.
    Aarch64,
    /// x86-64.
    X86_64,
}

/// ELF segment permission flags.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, RawReg)]
pub struct ElfPermissions(u8);

impl ElfPermissions {
    /// Executable permission flag.
    pub const EXECUTABLE: Self = Self(0x1);
    /// Writable permission flag.
    pub const WRITABLE: Self = Self(0x2);
    /// Readable permission flag.
    pub const READABLE: Self = Self(0x4);
    const MASK: u8 = 0b111;
}

/// Generic loadable segment metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadSegment {
    /// Segment permissions from `p_flags`.
    pub flags: ElfPermissions,
    /// File offset of bytes to load.
    pub file_offset: u64,
    /// Number of bytes supplied by the file.
    pub file_size: u64,
    /// Number of bytes occupied in memory after zero-fill.
    pub mem_size: u64,
    /// Virtual destination address.
    pub vaddr: u64,
    /// Physical destination address.
    pub paddr: u64,
    /// Required segment alignment.
    pub align: u64,
}

/// Summary metadata for an executable image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageInfo {
    /// ELF word size class.
    pub class: ElfClass,
    /// ELF machine type.
    pub machine: ElfMachine,
    /// ELF entry address exactly as encoded in the header.
    pub entry: u64,
    /// Lowest physical address covered by a load segment.
    pub load_min: u64,
    /// Exclusive upper physical address covered by a load segment.
    pub load_max: u64,
    /// Number of `PT_LOAD` program headers.
    pub segment_count: usize,
}

/// Program header data extracted from an ELF64 file.
///
/// This compatibility type intentionally retains the existing ELF64 callback
/// API. New code can use [`LoadSegment`] directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgramHeaderData {
    permission: ElfPermissions,
    address: u64,
    file_len: u64,
    mem_len: u64,
    offset: u64,
    align: u64,
}

impl ProgramHeaderData {
    /// Returns segment permissions from `p_flags`.
    pub const fn permissions(&self) -> ElfPermissions {
        self.permission
    }

    /// Returns the physical load address from `p_paddr`.
    pub const fn address(&self) -> u64 {
        self.address
    }

    /// Returns the file-backed byte count from `p_filesz`.
    pub const fn file_len(&self) -> u64 {
        self.file_len
    }

    /// Returns the in-memory byte count from `p_memsz`.
    pub const fn mem_len(&self) -> u64 {
        self.mem_len
    }

    /// Returns the source file offset from `p_offset`.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the requested segment alignment from `p_align`.
    pub const fn align(&self) -> u64 {
        self.align
    }
}

/// Parses a 32-bit, little-endian ARM ELF executable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Elf32<'a> {
    data: &'a [u8],
    phoff: usize,
    phnum: usize,
    entry: u32,
}

impl<'a> Elf32<'a> {
    /// Parses an ELF32 ARM executable without requiring input alignment.
    pub fn parse_arm_le(data: &'a [u8]) -> Result<Self, ElfErr> {
        validate_ident(data, ElfClass::Elf32, ElfEndian::Little)?;
        if read_u16_le(data, 16)? != ET_EXEC
            || read_u16_le(data, 18)? != EM_ARM
            || read_u32_le(data, 20)? != EV_CURRENT
        {
            return Err(ElfErr::Unsupported);
        }
        if read_u16_le(data, 40)? as usize != ELF32_HEADER_LEN
            || read_u16_le(data, 42)? as usize != ELF32_PROGRAM_HEADER_LEN
        {
            return Err(ElfErr::Invalid);
        }
        let phoff = usize_from_u32(read_u32_le(data, 28)?)?;
        let phnum = usize::from(read_u16_le(data, 44)?);
        checked_table_end(phoff, phnum, ELF32_PROGRAM_HEADER_LEN, data.len())?;
        let elf = Self {
            data,
            phoff,
            phnum,
            entry: read_u32_le(data, 24)?,
        };
        elf.validate_program_headers()?;
        Ok(elf)
    }

    /// Returns the ELF entry address exactly as encoded in `e_entry`.
    pub const fn entry(&self) -> Result<u32, ElfErr> {
        Ok(self.entry)
    }

    /// Calls `f` for every validated `PT_LOAD` program header.
    pub fn for_each_load_segment(
        &self,
        mut f: impl FnMut(LoadSegment) -> Result<(), ElfErr>,
    ) -> Result<(), ElfErr> {
        for index in 0..self.phnum {
            let segment = self.program_header(index)?;
            match segment_type(self.data, self.program_header_offset(index)?)? {
                PT_LOAD => f(segment)?,
                PT_INTERP | PT_DYNAMIC => return Err(ElfErr::Invalid),
                PT_NULL | PT_NOTE | PT_PHDR => {}
                _ => {}
            }
        }
        Ok(())
    }

    /// Returns summary metadata for the validated executable.
    pub fn image_info(&self) -> Result<ImageInfo, ElfErr> {
        let mut load_min = u64::MAX;
        let mut load_max = 0u64;
        let mut segment_count = 0usize;
        self.for_each_load_segment(|segment| {
            let end = segment
                .paddr
                .checked_add(segment.mem_size)
                .ok_or(ElfErr::Invalid)?;
            load_min = min(load_min, segment.paddr);
            load_max = load_max.max(end);
            segment_count = segment_count.checked_add(1).ok_or(ElfErr::Invalid)?;
            Ok(())
        })?;
        if segment_count == 0 {
            load_min = 0;
        }
        Ok(ImageInfo {
            class: ElfClass::Elf32,
            machine: ElfMachine::Arm,
            entry: u64::from(self.entry),
            load_min,
            load_max,
            segment_count,
        })
    }

    fn validate_program_headers(&self) -> Result<(), ElfErr> {
        self.for_each_load_segment(|_| Ok(()))
    }

    fn program_header(&self, index: usize) -> Result<LoadSegment, ElfErr> {
        let offset = self.program_header_offset(index)?;
        let file_offset = u64::from(read_u32_le(self.data, offset + 4)?);
        let vaddr = u64::from(read_u32_le(self.data, offset + 8)?);
        let paddr = u64::from(read_u32_le(self.data, offset + 12)?);
        let file_size = u64::from(read_u32_le(self.data, offset + 16)?);
        let mem_size = u64::from(read_u32_le(self.data, offset + 20)?);
        let flags =
            ElfPermissions((read_u32_le(self.data, offset + 24)? as u8) & ElfPermissions::MASK);
        let align = u64::from(read_u32_le(self.data, offset + 28)?);
        validate_segment(
            self.data.len(),
            file_offset,
            file_size,
            mem_size,
            vaddr,
            align,
            true,
        )?;
        Ok(LoadSegment {
            flags,
            file_offset,
            file_size,
            mem_size,
            vaddr,
            paddr,
            align,
        })
    }

    fn program_header_offset(&self, index: usize) -> Result<usize, ElfErr> {
        if index >= self.phnum {
            return Err(ElfErr::Invalid);
        }
        self.phoff
            .checked_add(
                index
                    .checked_mul(ELF32_PROGRAM_HEADER_LEN)
                    .ok_or(ElfErr::Invalid)?,
            )
            .ok_or(ElfErr::Invalid)
    }

    fn validate_load_ranges_disjoint(&self) -> Result<(), ElfErr> {
        for right in 0..self.phnum {
            let right_offset = self.program_header_offset(right)?;
            if segment_type(self.data, right_offset)? != PT_LOAD {
                continue;
            }
            let right_segment = self.program_header(right)?;
            let right_end = segment_end_for(right_segment)?;
            for left in 0..right {
                let left_offset = self.program_header_offset(left)?;
                if segment_type(self.data, left_offset)? != PT_LOAD {
                    continue;
                }
                let left_segment = self.program_header(left)?;
                if ranges_overlap(
                    left_segment.paddr,
                    segment_end_for(left_segment)?,
                    right_segment.paddr,
                    right_end,
                ) {
                    return Err(ElfErr::Invalid);
                }
            }
        }
        Ok(())
    }
}

/// Options for contiguous image materialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializeOptions {
    /// Physical address corresponding to output byte zero.
    pub load_base: u64,
    /// Maximum materialized image size, independent of output capacity.
    pub max_image_size: usize,
    /// Requires `e_entry` to be inside the materialized contiguous range.
    pub require_entry_in_range: bool,
}

/// Metadata for a caller-provided materialized image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializedImage {
    /// Physical address corresponding to output byte zero.
    pub load_base: u64,
    /// Number of deterministic bytes in the contiguous output image.
    pub image_len: usize,
    /// ELF entry address without architecture-specific adjustment.
    pub entry: u64,
    /// Stack metadata when the ELF parser has an explicit source for it.
    pub stack: Option<u64>,
}

/// Materializes ELF32 little-endian ARM `PT_LOAD` segments into `output`.
///
/// Segment destinations use `p_paddr`. The generic loader deliberately leaves
/// entry-bit and stack policy to its caller.
pub fn materialize_elf32_arm_le(
    elf: &[u8],
    output: &mut [u8],
    options: MaterializeOptions,
) -> Result<MaterializedImage, ElfErr> {
    let parsed = Elf32::parse_arm_le(elf)?;
    let max_end = options
        .load_base
        .checked_add(options.max_image_size as u64)
        .ok_or(ElfErr::Invalid)?;
    let usable_len = min(output.len(), options.max_image_size);
    output[..usable_len].fill(0);
    parsed.validate_load_ranges_disjoint()?;

    let mut image_end = options.load_base;
    let mut segment_count = 0usize;
    parsed.for_each_load_segment(|segment| {
        let segment_end = segment
            .paddr
            .checked_add(segment.mem_size)
            .ok_or(ElfErr::Invalid)?;
        if segment.paddr < options.load_base || segment_end > max_end {
            return Err(ElfErr::Invalid);
        }
        let dst_offset = usize_from_u64(segment.paddr - options.load_base)?;
        let mem_len = usize_from_u64(segment.mem_size)?;
        let dst_end = dst_offset.checked_add(mem_len).ok_or(ElfErr::Invalid)?;
        if dst_end > usable_len {
            return Err(ElfErr::TooShort);
        }
        let source_start = usize_from_u64(segment.file_offset)?;
        let source_len = usize_from_u64(segment.file_size)?;
        let source_end = source_start
            .checked_add(source_len)
            .ok_or(ElfErr::Invalid)?;
        let source = elf.get(source_start..source_end).ok_or(ElfErr::TooShort)?;
        output[dst_offset..dst_offset + source_len].copy_from_slice(source);
        image_end = image_end.max(segment_end);
        segment_count = segment_count.checked_add(1).ok_or(ElfErr::Invalid)?;
        Ok(())
    })?;

    let image_len = usize_from_u64(image_end - options.load_base)?;
    if segment_count == 0 || image_len == 0 {
        return Err(ElfErr::Invalid);
    }
    if options.require_entry_in_range
        && (u64::from(parsed.entry) < options.load_base || u64::from(parsed.entry) >= image_end)
    {
        return Err(ElfErr::Invalid);
    }
    Ok(MaterializedImage {
        load_base: options.load_base,
        image_len,
        entry: u64::from(parsed.entry),
        stack: None,
    })
}

/// Parsed ELF64 file retained for API compatibility.
#[derive(Debug)]
pub struct Elf64<'a> {
    data: &'a [u8],
    endian: ElfEndian,
    phoff: usize,
    phnum: usize,
}

impl<'a> Elf64<'a> {
    /// Returns the legacy ELF64 header size and alignment requirement.
    pub const fn elf_header_size() -> (usize, usize) {
        (ELF64_HEADER_LEN, core::mem::align_of::<u64>())
    }

    /// Returns the byte range occupied by the ELF64 header and program table.
    pub fn elf_real_header_size(&self) -> (usize, usize) {
        (
            self.phoff
                .saturating_add(self.phnum.saturating_mul(ELF64_PROGRAM_HEADER_LEN)),
            core::mem::align_of::<u64>(),
        )
    }

    /// Parses an ELF64 executable without dereferencing aligned ELF structs.
    ///
    /// # Safety
    ///
    /// Kept unsafe solely for source compatibility with existing callers. This
    /// implementation performs only bounds-checked byte reads and does not
    /// require any pointer alignment or additional caller invariant.
    pub unsafe fn new(elf: &'a [u8]) -> Result<Self, ElfErr> {
        validate_ident_64(elf)?;
        let endian = match elf.get(5).copied() {
            Some(ELFDATA2LSB) => ElfEndian::Little,
            Some(ELFDATA2MSB) => ElfEndian::Big,
            _ => return Err(ElfErr::Unsupported),
        };
        if read_u16(elf, 16, endian)? != ET_EXEC || read_u32(elf, 20, endian)? != EV_CURRENT {
            return Err(ElfErr::Unsupported);
        }
        let machine = read_u16(elf, 18, endian)?;
        if !machine_supported_on_current_target(machine) {
            return Err(ElfErr::Unsupported);
        }
        if usize::from(read_u16(elf, 52, endian)?) != ELF64_HEADER_LEN
            || usize::from(read_u16(elf, 54, endian)?) != ELF64_PROGRAM_HEADER_LEN
        {
            return Err(ElfErr::Invalid);
        }
        let phoff = usize_from_u64(read_u64(elf, 32, endian)?)?;
        let phnum = usize::from(read_u16(elf, 56, endian)?);
        checked_table_end(phoff, phnum, ELF64_PROGRAM_HEADER_LEN, elf.len())?;
        Ok(Self {
            data: elf,
            endian,
            phoff,
            phnum,
        })
    }

    /// Iterates over loadable ELF64 program headers, invoking `f` for each.
    pub fn iterate_program_header<F>(&self, mut f: F) -> Result<(), ElfErr>
    where
        F: FnMut(&ProgramHeaderData),
    {
        for index in 0..self.phnum {
            let offset = self
                .phoff
                .checked_add(
                    index
                        .checked_mul(ELF64_PROGRAM_HEADER_LEN)
                        .ok_or(ElfErr::Invalid)?,
                )
                .ok_or(ElfErr::Invalid)?;
            let p_type = read_u32(self.data, offset, self.endian)?;
            if p_type == PT_INTERP || p_type == PT_DYNAMIC {
                return Err(ElfErr::Invalid);
            }
            if p_type != PT_LOAD {
                continue;
            }
            let file_offset = read_u64(self.data, offset + 8, self.endian)?;
            let vaddr = read_u64(self.data, offset + 16, self.endian)?;
            let address = read_u64(self.data, offset + 24, self.endian)?;
            let file_len = read_u64(self.data, offset + 32, self.endian)?;
            let mem_len = read_u64(self.data, offset + 40, self.endian)?;
            let align = read_u64(self.data, offset + 48, self.endian)?;
            validate_segment(
                self.data.len(),
                file_offset,
                file_len,
                mem_len,
                vaddr,
                align,
                false,
            )?;
            let data = ProgramHeaderData {
                permission: ElfPermissions(
                    (read_u32(self.data, offset + 4, self.endian)? as u8) & ElfPermissions::MASK,
                ),
                address,
                file_len,
                mem_len,
                offset: file_offset,
                align,
            };
            f(&data);
        }
        Ok(())
    }
}

/// Returns whether `data` starts with the ELF magic number.
pub fn is_elf(data: &[u8]) -> bool {
    data.get(..4) == Some(&[0x7f, b'E', b'L', b'F'])
}

/// Detects an ELF word-size class from a validated ELF identifier.
pub fn elf_class(data: &[u8]) -> Result<ElfClass, ElfErr> {
    if data.len() < 5 {
        return Err(ElfErr::TooShort);
    }
    if !is_elf(data) {
        return Err(ElfErr::InvalidMagic);
    }
    match data[4] {
        ELFCLASS32 => Ok(ElfClass::Elf32),
        ELFCLASS64 => Ok(ElfClass::Elf64),
        _ => Err(ElfErr::Unsupported),
    }
}

fn validate_ident(data: &[u8], class: ElfClass, endian: ElfEndian) -> Result<(), ElfErr> {
    if data.len() < ELF_IDENT_LEN {
        return Err(ElfErr::TooShort);
    }
    if elf_class(data)? != class {
        return Err(ElfErr::Unsupported);
    }
    if data[5] != ELFDATA2LSB || !matches!(endian, ElfEndian::Little) || data[6] != EV_CURRENT as u8
    {
        return Err(ElfErr::Unsupported);
    }
    Ok(())
}

fn validate_ident_64(data: &[u8]) -> Result<(), ElfErr> {
    if data.len() < ELF_IDENT_LEN {
        return Err(ElfErr::TooShort);
    }
    if elf_class(data)? != ElfClass::Elf64 || data[6] != EV_CURRENT as u8 {
        return Err(ElfErr::Unsupported);
    }
    Ok(())
}

fn machine_supported_on_current_target(machine: u16) -> bool {
    match machine {
        EM_X86_64 => cfg!(target_arch = "x86_64"),
        EM_AARCH64 => cfg!(target_arch = "aarch64"),
        _ => false,
    }
}

fn validate_segment(
    file_len: usize,
    offset: u64,
    file_size: u64,
    mem_size: u64,
    vaddr: u64,
    align: u64,
    require_power_of_two_align: bool,
) -> Result<(), ElfErr> {
    if file_size > mem_size {
        return Err(ElfErr::Invalid);
    }
    let end = offset.checked_add(file_size).ok_or(ElfErr::Invalid)?;
    if end > file_len as u64 {
        return Err(ElfErr::TooShort);
    }
    if require_power_of_two_align && align > 1 && !align.is_power_of_two() {
        return Err(ElfErr::Invalid);
    }
    if align > 1 && vaddr % align != offset % align {
        return Err(ElfErr::Invalid);
    }
    Ok(())
}

fn segment_type(data: &[u8], offset: usize) -> Result<u32, ElfErr> {
    read_u32_le(data, offset)
}

fn segment_end_for(segment: LoadSegment) -> Result<u64, ElfErr> {
    segment
        .paddr
        .checked_add(segment.mem_size)
        .ok_or(ElfErr::Invalid)
}

fn ranges_overlap(start_a: u64, end_a: u64, start_b: u64, end_b: u64) -> bool {
    start_a < end_b && start_b < end_a
}

fn checked_table_end(
    offset: usize,
    count: usize,
    entry_len: usize,
    file_len: usize,
) -> Result<(), ElfErr> {
    let bytes = count.checked_mul(entry_len).ok_or(ElfErr::Invalid)?;
    let end = offset.checked_add(bytes).ok_or(ElfErr::Invalid)?;
    if end > file_len {
        return Err(ElfErr::TooShort);
    }
    Ok(())
}

fn usize_from_u32(value: u32) -> Result<usize, ElfErr> {
    usize::try_from(value).map_err(|_| ElfErr::Invalid)
}

fn usize_from_u64(value: u64) -> Result<usize, ElfErr> {
    usize::try_from(value).map_err(|_| ElfErr::Invalid)
}

fn read_u16_le(data: &[u8], offset: usize) -> Result<u16, ElfErr> {
    read_u16(data, offset, ElfEndian::Little)
}

fn read_u32_le(data: &[u8], offset: usize) -> Result<u32, ElfErr> {
    read_u32(data, offset, ElfEndian::Little)
}

fn read_u16(data: &[u8], offset: usize, endian: ElfEndian) -> Result<u16, ElfErr> {
    let bytes = data
        .get(offset..offset.checked_add(2).ok_or(ElfErr::Invalid)?)
        .ok_or(ElfErr::TooShort)?;
    Ok(match endian {
        ElfEndian::Little => u16::from_le_bytes([bytes[0], bytes[1]]),
        ElfEndian::Big => u16::from_be_bytes([bytes[0], bytes[1]]),
    })
}

fn read_u32(data: &[u8], offset: usize, endian: ElfEndian) -> Result<u32, ElfErr> {
    let bytes = data
        .get(offset..offset.checked_add(4).ok_or(ElfErr::Invalid)?)
        .ok_or(ElfErr::TooShort)?;
    Ok(match endian {
        ElfEndian::Little => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        ElfEndian::Big => u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
    })
}

fn read_u64(data: &[u8], offset: usize, endian: ElfEndian) -> Result<u64, ElfErr> {
    let bytes = data
        .get(offset..offset.checked_add(8).ok_or(ElfErr::Invalid)?)
        .ok_or(ElfErr::TooShort)?;
    Ok(match endian {
        ElfEndian::Little => u64::from_le_bytes(bytes.try_into().map_err(|_| ElfErr::TooShort)?),
        ElfEndian::Big => u64::from_be_bytes(bytes.try_into().map_err(|_| ElfErr::TooShort)?),
    })
}

#[derive(Clone, Copy, Debug)]
enum ElfEndian {
    Big,
    Little,
}

/// ELF parsing error types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ElfErr {
    /// Input data is too short to contain a required ELF structure.
    TooShort,
    /// ELF magic number is invalid.
    InvalidMagic,
    /// ELF format is not supported by the selected parser.
    Unsupported,
    /// ELF structure is malformed or internally inconsistent.
    Invalid,
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 0x2000_0000;

    fn write_u16(data: &mut [u8], offset: usize, value: u16) {
        data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn elf32_with_headers(headers: &[[u32; 8]], len: usize) -> [u8; 512] {
        let mut data = [0u8; 512];
        let len = len.min(data.len());
        data[0..4].copy_from_slice(b"\x7fELF");
        data[4] = ELFCLASS32;
        data[5] = ELFDATA2LSB;
        data[6] = 1;
        write_u16(&mut data, 16, ET_EXEC);
        write_u16(&mut data, 18, EM_ARM);
        write_u32(&mut data, 20, 1);
        write_u32(&mut data, 24, BASE + 1);
        write_u32(&mut data, 28, ELF32_HEADER_LEN as u32);
        write_u16(&mut data, 40, ELF32_HEADER_LEN as u16);
        write_u16(&mut data, 42, ELF32_PROGRAM_HEADER_LEN as u16);
        write_u16(&mut data, 44, headers.len() as u16);
        for (index, header) in headers.iter().enumerate() {
            let offset = ELF32_HEADER_LEN + index * ELF32_PROGRAM_HEADER_LEN;
            for (word, value) in header.iter().enumerate() {
                write_u32(&mut data, offset + word * 4, *value);
            }
        }
        let _ = len;
        data
    }

    fn load(offset: u32, paddr: u32, file_size: u32, mem_size: u32) -> [u32; 8] {
        [PT_LOAD, offset, paddr, paddr, file_size, mem_size, 5, 4]
    }

    #[test]
    fn parses_valid_minimal_elf32_arm() {
        let mut data = elf32_with_headers(&[load(0x100, BASE, 4, 4)], 512);
        data[0x100..0x104].copy_from_slice(&[1, 2, 3, 4]);
        let elf = Elf32::parse_arm_le(&data).unwrap();
        assert_eq!(elf.entry(), Ok(BASE + 1));
        assert_eq!(elf.image_info().unwrap().segment_count, 1);
    }

    #[test]
    fn rejects_invalid_magic() {
        assert_eq!(
            Elf32::parse_arm_le(&[0; ELF32_HEADER_LEN]),
            Err(ElfErr::InvalidMagic)
        );
    }

    #[test]
    fn rejects_unsupported_class() {
        let mut data = elf32_with_headers(&[], 512);
        data[4] = ELFCLASS64;
        assert_eq!(Elf32::parse_arm_le(&data), Err(ElfErr::Unsupported));
    }

    #[test]
    fn rejects_big_endian() {
        let mut data = elf32_with_headers(&[], 512);
        data[5] = ELFDATA2MSB;
        assert_eq!(Elf32::parse_arm_le(&data), Err(ElfErr::Unsupported));
    }

    #[test]
    fn rejects_wrong_machine_and_type() {
        let mut wrong_machine = elf32_with_headers(&[], 512);
        write_u16(&mut wrong_machine, 18, EM_AARCH64);
        assert_eq!(
            Elf32::parse_arm_le(&wrong_machine),
            Err(ElfErr::Unsupported)
        );
        let mut wrong_type = elf32_with_headers(&[], 512);
        write_u16(&mut wrong_type, 16, 3);
        assert_eq!(Elf32::parse_arm_le(&wrong_type), Err(ElfErr::Unsupported));
    }

    #[test]
    fn rejects_invalid_segment_sizes_and_offsets() {
        let sizes = elf32_with_headers(&[load(0x100, BASE, 8, 4)], 512);
        assert_eq!(Elf32::parse_arm_le(&sizes), Err(ElfErr::Invalid));
        let out_of_bounds = elf32_with_headers(&[load(510, BASE, 4, 4)], 512);
        assert_eq!(Elf32::parse_arm_le(&out_of_bounds), Err(ElfErr::TooShort));
    }

    #[test]
    fn rejects_program_table_overflow_and_forbidden_headers() {
        let mut overflow = elf32_with_headers(&[], 512);
        write_u32(&mut overflow, 28, u32::MAX);
        write_u16(&mut overflow, 44, 2);
        assert_eq!(Elf32::parse_arm_le(&overflow), Err(ElfErr::TooShort));
        for kind in [PT_INTERP, PT_DYNAMIC] {
            let data = elf32_with_headers(&[[kind, 0, 0, 0, 0, 0, 0, 0]], 512);
            assert_eq!(Elf32::parse_arm_le(&data), Err(ElfErr::Invalid));
        }
    }

    #[test]
    fn materializes_load_segments_holes_and_bss() {
        let mut data =
            elf32_with_headers(&[load(0x100, BASE, 2, 4), load(0x110, BASE + 8, 2, 2)], 512);
        data[0x100..0x102].copy_from_slice(&[0xaa, 0xbb]);
        data[0x110..0x112].copy_from_slice(&[0xcc, 0xdd]);
        let mut output = [0xff; 16];
        let output_len = output.len();
        let image = materialize_elf32_arm_le(
            &data,
            &mut output,
            MaterializeOptions {
                load_base: u64::from(BASE),
                max_image_size: output_len,
                require_entry_in_range: true,
            },
        )
        .unwrap();
        assert_eq!(image.image_len, 10);
        assert_eq!(&output[..10], &[0xaa, 0xbb, 0, 0, 0, 0, 0, 0, 0xcc, 0xdd]);
    }

    #[test]
    fn materializes_one_load_segment() {
        let mut data = elf32_with_headers(&[load(0x100, BASE, 3, 3)], 512);
        data[0x100..0x103].copy_from_slice(&[7, 8, 9]);
        let mut output = [0u8; 8];
        let image = materialize_elf32_arm_le(
            &data,
            &mut output,
            MaterializeOptions {
                load_base: u64::from(BASE),
                max_image_size: 8,
                require_entry_in_range: true,
            },
        )
        .unwrap();
        assert_eq!(image.image_len, 3);
        assert_eq!(&output[..3], &[7, 8, 9]);
    }

    #[test]
    fn rejects_materialization_range_and_overlap_errors() {
        let below = elf32_with_headers(&[load(0x100, BASE - 4, 1, 1)], 512);
        let mut output = [0; 8];
        let options = MaterializeOptions {
            load_base: u64::from(BASE),
            max_image_size: output.len(),
            require_entry_in_range: true,
        };
        assert_eq!(
            materialize_elf32_arm_le(&below, &mut output, options),
            Err(ElfErr::Invalid)
        );
        let beyond = elf32_with_headers(&[load(0x100, BASE + 8, 1, 1)], 512);
        assert_eq!(
            materialize_elf32_arm_le(&beyond, &mut output, options),
            Err(ElfErr::Invalid)
        );
        let overlap =
            elf32_with_headers(&[load(0x100, BASE, 1, 4), load(0x110, BASE + 2, 1, 4)], 512);
        assert_eq!(
            materialize_elf32_arm_le(&overlap, &mut output, options),
            Err(ElfErr::Invalid)
        );
    }

    #[test]
    fn accepts_an_unaligned_input_slice() {
        let data = elf32_with_headers(&[load(0x100, BASE, 1, 1)], 512);
        let mut unaligned = [0u8; 513];
        unaligned[1..].copy_from_slice(&data);
        assert!(Elf32::parse_arm_le(&unaligned[1..]).is_ok());
    }
}
