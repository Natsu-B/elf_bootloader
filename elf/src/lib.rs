#![no_std]
#![allow(unused)]

use core::cmp::min;

use typestate::RawReg;
use typestate_macro::RawReg;
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(size_of::<Elf64Header>() == 64);
const _: () = assert!(size_of::<Elf64ProgramHeader>() == 56);

type Elf64Addr = u64;
type Elf64Off = u64;
type Elf64Half = u16;
type Elf64Word = u32;
type Elf64Sword = i32;
type Elf64Xword = u64;
type Elf64Sxword = i64;

#[repr(C)]
struct Elf64Header {
    e_ident: ElfHeaderIdent,   // elf identification
    e_type: Elf64Half,         // Object File Type
    e_machine: ElfMachineType, // Machine Type
    e_version: Elf64Word,      // Object File Version
    e_entry: Elf64Addr,        // Entry Point Address
    e_phoff: Elf64Off,         // Program Header Offset
    e_shoff: Elf64Off,         // Section Header Offset
    e_flags: Elf64Word,        // Processor Specific Flags
    e_ehsize: Elf64Half,       // ELF Header Size
    e_phentsize: Elf64Half,    // Size Of Program Header Entry
    e_phnum: Elf64Half,        // Number Of Program Header Entries
    e_shentsize: Elf64Half,    // Size Of Section Header Entry
    e_shnum: Elf64Half,        // Number Of Section Header Entries
    e_shstrndx: Elf64Half,     // Section Name String Table Index
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, RawReg, PartialEq, Eq)]
struct ElfMachineType(Elf64Half);

impl ElfMachineType {
    const EM_X86_64: Self = Self(62);
    const EM_AARCH64: Self = Self(183);
}

#[repr(C)]
struct ElfHeaderIdent {
    magic: [u8; 4],  // File Identification
    class: u8,       // File Class
    data: u8,        // Data Encoding
    version: u8,     // File Version
    os_abi: u8,      // OS/ABI Identification
    abi_version: u8, // ABI Version
    _reserved: [u8; 7],
}

#[repr(C)]
struct Elf64ProgramHeader {
    p_type: ElfProgramHeaderTypes, // Type Of Segment
    p_flags: Elf64Word,            // Segment Attributes
    p_offset: Elf64Off,            // Offset In File
    p_vaddr: Elf64Addr,            // Virtual Address In Memory
    p_paddr: Elf64Addr,            // Physical Address In Memory
    p_filesz: Elf64Xword,          // Size Of Segment In File
    p_memsz: Elf64Xword,           // Size Of Segment In Memory
    p_align: Elf64Xword,           // Alignment Of Segment
}

#[repr(transparent)]
#[derive(Clone, Copy, RawReg, PartialEq)]
struct ElfProgramHeaderTypes(Elf64Word);

impl ElfProgramHeaderTypes {
    const PT_NULL: Self = Self(0);
    const PT_LOAD: Self = Self(1);
    const PT_DYNAMIC: Self = Self(2);
    const PT_INTERP: Self = Self(3);
    const PT_NOTE: Self = Self(4);
    const PT_SHLIB: Self = Self(5);
    const PT_PHDR: Self = Self(6);
}

#[derive(Clone, Copy, Debug)]
enum ElfEndian {
    Big,
    Little,
}

#[derive(Clone, Copy, Debug)]
pub struct ProgramHeaderData {
    /// Segment permissions derived from `p_flags`.
    /// Only the lower 3 bits are meaningful: PF_X=0x1, PF_W=0x2, PF_R=0x4.
    permission: ElfPermissions,

    /// Destination load address for this segment on bare metal.
    /// In this implementation we use `p_paddr` (physical address).
    /// Many toolchains set `p_paddr == p_vaddr`, but they are allowed to differ.
    address: u64,

    /// Number of bytes to copy from the file into memory for this segment.
    /// Comes from `p_filesz`.
    file_len: u64,

    /// Total size of the segment in memory, including any zero-filled tail (.bss).
    /// Comes from `p_memsz` and must be >= `file_len`.
    mem_len: u64,

    /// File offset where this segment’s data begins.
    /// Comes from `p_offset`. Must satisfy alignment relation with `vaddr`:
    /// if `p_align > 0`, then `p_vaddr % p_align == p_offset % p_align`.
    offset: u64,

    /// Required alignment for the segment (from `p_align`), in bytes.
    /// If zero, callers may choose a sensible default (e.g., page size).
    align: u64,
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, RawReg)]
pub struct ElfPermissions(u8);

impl ElfPermissions {
    pub const EXECUTABLE: Self = Self(0x1);
    pub const WRITABLE: Self = Self(0x2);
    pub const READABLE: Self = Self(0x4);
    const MASK: Self = Self(0b111);
}

#[derive(Debug)]
pub struct Elf64<'a> {
    data: &'a [u8],
    endian: ElfEndian,
}

impl<'a> Elf64<'a> {
    /// return size and require alignment
    pub const fn elf_header_size() -> (usize /* size */, usize /* alignment */) {
        (size_of::<Elf64Header>(), align_of::<Elf64Header>())
    }

    pub fn elf_real_header_size(&self) -> (usize /* size */, usize /* alignment */) {
        let header = unsafe { &*(self.data.as_ptr() as *const Elf64Header) };
        (
            read(header.e_phoff, self.endian) as usize
                + size_of::<Elf64ProgramHeader>() * read(header.e_phnum, self.endian) as usize,
            align_of::<Elf64Header>(),
        )
    }

    /// # Safety
    ///  require
    ///  - slice head points elf header
    ///  - slice size is lager than elf_header_size().0 /* size */
    ///  - slice head is algined elf_header_size().1 /* alignment */
    pub unsafe fn new(elf: &'a [u8]) -> Result<Self, ElfErr> {
        if elf.len() < Self::elf_header_size().0 {
            return Err(ElfErr::TooShort);
        }
        if elf.get(0..4) != Some(&[0x7F, b'E', b'L', b'F']) {
            return Err(ElfErr::InvalidMagic);
        }
        let header = unsafe { &*(elf.as_ptr() as *const Elf64Header) };
        if header.e_ident.class != 2 {
            // 64bit class is 2
            return Err(ElfErr::Unsupported);
        }
        let endian = match header.e_ident.data {
            1 => ElfEndian::Little,
            2 => ElfEndian::Big,
            _ => return Err(ElfErr::Unsupported),
        };
        if header.e_ident.version != 1 {
            // this program is ver1.4 elf specification compatible
            return Err(ElfErr::Unsupported);
        }
        let e_type = read(header.e_type, endian);
        if e_type != 2 {
            // 2: Executable File
            return Err(ElfErr::Unsupported);
        }
        match read(header.e_machine, endian) {
            ElfMachineType::EM_X86_64 if cfg!(target_arch = "x86_64") => {}
            ElfMachineType::EM_AARCH64 if cfg!(target_arch = "aarch64") => {}
            _ => return Err(ElfErr::Unsupported),
        }
        if read(header.e_version, endian) != 1 {
            return Err(ElfErr::Unsupported);
        }
        if read(header.e_ehsize, endian) != size_of::<Elf64Header>() as u16 {
            return Err(ElfErr::Invalid);
        }
        if read(header.e_phentsize, endian) != size_of::<Elf64ProgramHeader>() as u16 {
            return Err(ElfErr::Invalid);
        }
        Ok(Self { data: elf, endian })
    }

    pub fn iterate_program_header<F>(&self, mut f: F) -> Result<(), ElfErr>
    where
        F: FnMut(&ProgramHeaderData),
    {
        if self.data.len() < self.elf_real_header_size().0 {
            return Err(ElfErr::TooShort);
        }
        let header = unsafe { &*(self.data.as_ptr() as *const Elf64Header) };
        for i in 0..read(header.e_phnum, self.endian) as usize {
            let program_header = unsafe {
                &*((self.data.as_ptr() as usize
                    + read(header.e_phoff, self.endian) as usize
                    + i * size_of::<Elf64ProgramHeader>())
                    as *const Elf64ProgramHeader)
            };
            match read(program_header.p_type, self.endian) {
                ElfProgramHeaderTypes::PT_LOAD => {}
                ElfProgramHeaderTypes::PT_INTERP | ElfProgramHeaderTypes::PT_DYNAMIC => {
                    return Err(ElfErr::Invalid);
                }
                _ => continue,
            }
            let flags = ElfPermissions(read(program_header.p_flags, self.endian) as u8)
                & ElfPermissions::MASK;
            let address = read(program_header.p_paddr, self.endian);
            let v_address = read(program_header.p_vaddr, self.endian);
            let file_len = read(program_header.p_filesz, self.endian);
            let mem_len = read(program_header.p_memsz, self.endian);
            let align = read(program_header.p_align, self.endian);
            let offset = read(program_header.p_offset, self.endian);
            if align > 0 && (v_address % align) != (offset % align) {
                return Err(ElfErr::Invalid);
            }
            let end = offset.checked_add(file_len).ok_or(ElfErr::Invalid)?;
            if (end as usize) > self.data.len() {
                return Err(ElfErr::TooShort);
            }
            if file_len > mem_len {
                return Err(ElfErr::Invalid);
            }
            f(&ProgramHeaderData {
                permission: flags,
                address,
                file_len,
                mem_len,
                offset,
                align,
            });
        }
        Ok(())
    }
}

fn read<T: RawReg>(data: T, endian: ElfEndian) -> T {
    match endian {
        ElfEndian::Big => data.from_be(),
        ElfEndian::Little => data.from_le(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ElfErr {
    TooShort,
    InvalidMagic,
    Unsupported,
    Invalid,
}
