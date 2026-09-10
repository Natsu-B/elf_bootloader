//! Reserved SIPI bootstrap: real mode to an owner's private 64-bit host map.
//!
//! The caller reserves one executable low page and a separate below-4-GiB PML4.
//! No conventional low address is stolen. Firmware must have finished using APs
//! before any IPI is sent; this module only prepares the immutable handoff page.

use core::arch::global_asm;

/// Bounded CPU handoff records, keyed by full architectural APIC identity.
pub const MAX_CPUS: usize = 64;
/// CPUID in the explicit L1-visible real-mode probe confirms a carrier entry.
pub const READY_LEAF: u32 = 0x5448_4150;
const PAGE: usize = 4096;
const CONFIG: usize = 0x200;
const GDT_POINTER: usize = CONFIG;
const PROTECTED_POINTER: usize = CONFIG + 8;
const LONG_POINTER: usize = CONFIG + 16;
const ROOT: usize = CONFIG + 24;
const PAT: usize = CONFIG + 32;
const EFER: usize = CONFIG + 40;
const ENTRY: usize = CONFIG + 48;
const COUNT: usize = CONFIG + 56;
const SLOTS: usize = CONFIG + 64;
const SLOT_BYTES: usize = 32;
const GDT: usize = 0xb00;

// The code is copied before use, so every low address comes from the caller's
// reservation. ESI retains its physical base until the private root is active.
// No instruction uses a stack before the full APIC ID selects an owned slot.
// IF stays clear; all firmware service use ended before the caller sends SIPI.
// This is a data template, never an in-place 64-bit L0 function. Keeping it in
// read-only data also prevents treating real/32-bit bytes as 64-bit monitor ISA.
macro_rules! bootstrap_template {
($section:literal) => { global_asm!(
    $section,
    ".p2align 4",
    ".global thin_hv_ap_boot_start",
    ".global thin_hv_ap_boot_protected",
    ".global thin_hv_ap_boot_long",
    ".global thin_hv_ap_boot_probe",
    ".global thin_hv_ap_boot_end",
    ".code16",
    "thin_hv_ap_boot_start:",
    "cli", "cld",
    "xorl %eax, %eax", "movw %cs, %ax", "movw %ax, %ds", "movw %ax, %es",
    "shll $4, %eax", "movl %eax, %esi",
    "lgdtl {gdt_pointer}",
    "movl %cr0, %eax", "movl %eax, %ebp", "orl $1, %eax", "movl %eax, %cr0",
    "ljmpl *{protected_pointer}",
    ".code32",
    "thin_hv_ap_boot_protected:",
    "movw $0x10, %ax", "movw %ax, %ds", "movw %ax, %es", "movw %ax, %ss",
    // The shared PML4 uses the BSP's checked PAT encodings. Program PAT while
    // paging is still off and flush inherited cache state around the change.
    "movl %cr0, %eax", "andl $0xdfffffff, %eax", "orl $0x40000000, %eax",
    "movl %eax, %cr0", "wbinvd",
    "movl $0x277, %ecx", "rdmsr", "movl %eax, %edi", "movl %edx, %ebx",
    "movl {pat}(%esi), %eax", "movl {pat_high}(%esi), %edx",
    "wrmsr", "wbinvd",
    "movl $0x620, %eax", "movl %eax, %cr4",
    "movl {root}(%esi), %eax", "movl %eax, %cr3",
    "movl $0xc0000080, %ecx", "movl {efer}(%esi), %eax", "xorl %edx, %edx",
    "wrmsr",
    "movl $0x80000031, %eax", "movl %eax, %cr0",
    "ljmpl *{long_pointer}(%esi)",
    ".code64",
    "thin_hv_ap_boot_long:",
    "movl %esi, %esi",
    "movw $0x10, %ax", "movw %ax, %ds", "movw %ax, %es", "movw %ax, %ss",
    "xorl %eax, %eax", "movw %ax, %fs", "movw %ax, %gs",
    // INIT preserves cache-disable bits and PAT. Carry their original values
    // past our private paging transition; CPUID must not overwrite the copy.
    "movl %edi, %r12d", "movl %ebx, %r13d",
    "xorl %eax, %eax", "cpuid", "movl %eax, %r8d",
    "cmpl $0x1f, %r8d", "jb 2f",
    "movl $0x1f, %eax", "xorl %ecx, %ecx", "cpuid", "testl %ebx, %ebx", "jnz 4f",
    "2:", "cmpl $0xb, %r8d", "jb 3f",
    "movl $0xb, %eax", "xorl %ecx, %ecx", "cpuid", "testl %ebx, %ebx", "jnz 4f",
    "3:", "movl $1, %eax", "cpuid", "movl %ebx, %edx", "shrl $24, %edx",
    "4:", "movl {count}(%rsi), %ecx", "leaq {slots}(%rsi), %r9",
    "5:", "testl %ecx, %ecx", "jz 7f",
    "cmpl (%r9), %edx", "je 6f", "addq $32, %r9", "decl %ecx", "jmp 5b",
    "6:", "movq 24(%r9), %rax", "movq %rax, %cr3",
    "movq 8(%r9), %rsp", "movq 16(%r9), %rdi",
    "movq {entry}(%rsi), %rax", "movl %ebp, %esi",
    "movq %r13, %rdx", "shlq $32, %rdx", "orq %r12, %rdx",
    "xorl %ebp, %ebp", "jmp *%rax",
    "7:", "cli", "hlt", "jmp 7b",
    ".code16",
    "thin_hv_ap_boot_probe:",
    "movl ${ready}, %eax", "cpuid",
    "8:", "cli", "hlt", "jmp 8b",
    "thin_hv_ap_boot_end:",
    ".code64",
    ".popsection",
    gdt_pointer = const GDT_POINTER,
    protected_pointer = const PROTECTED_POINTER,
    long_pointer = const LONG_POINTER,
    root = const ROOT,
    pat = const PAT,
    pat_high = const PAT + 4,
    efer = const EFER,
    entry = const ENTRY,
    count = const COUNT,
    slots = const SLOTS,
    ready = const READY_LEAF,
    options(att_syntax),
); };
}

#[cfg(target_os = "uefi")]
bootstrap_template!(".pushsection .rdata,\"dr\"");
#[cfg(not(target_os = "uefi"))]
bootstrap_template!(".pushsection .rodata,\"a\"");

unsafe extern "C" {
    static thin_hv_ap_boot_start: u8;
    static thin_hv_ap_boot_protected: u8;
    static thin_hv_ap_boot_long: u8;
    static thin_hv_ap_boot_probe: u8;
    static thin_hv_ap_boot_end: u8;
}

/// Immutable addresses for one CPU. The stack uses SysV entry alignment (8 mod
/// 16); `argument` is passed in RDI and `host_cr3` replaces the shared boot root.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    /// Full CPUID.1F/0B APIC identity, not a firmware processor index.
    pub apic_id: u32,
    /// Top of exclusively reserved initial stack, with a zero return slot.
    pub stack: u64,
    /// Monitor-owned preparation record for this CPU.
    pub argument: u64,
    /// Private four-level host PML4, without PCID bits.
    pub host_cr3: u64,
}

/// Invalid preparation never publishes a partial executable bootstrap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The complete SIPI page must lie below 1 MiB, not at address zero.
    LowPage,
    /// At least one and at most MAX_CPUS distinct CPU records are required.
    CpuRecords,
    /// Bootstrap code, stack or host-root addresses are unsupported.
    Address,
    /// PAT or pre-paging EFER does not match the supported boot contract.
    MemoryType,
    /// Linked assembly no longer fits its bounded reserved code region.
    CodeLayout,
}

/// Initializes one low SIPI page after all addresses and bytes are validated.
///
/// This writes only `output`; it does not claim CPU ownership or execute IPIs.
/// The caller must make the page executable WB RAM and preserve it, all stacks,
/// roots, arguments and entry code until all participating CPUs have retired.
/// `bootstrap_cr3` must be a private PML4 below 4 GiB mapping these same objects.
/// `efer` supplies only SCE/LME/NXE (LMA is set later by hardware).
/// Returns the L1-visible real-mode CPUID probe offset in this page.
pub fn prepare(
    output: &mut [u8; PAGE],
    physical: u64,
    bootstrap_cr3: u64,
    pat: u64,
    efer: u64,
    entry: u64,
    slots: &[Slot],
) -> Result<u16, Error> {
    if physical == 0 || physical >= 1 << 20 || physical & 4095 != 0 {
        return Err(Error::LowPage);
    }
    let low_canonical = |address: u64| address != 0 && address < 1 << 47;
    if bootstrap_cr3 == 0
        || bootstrap_cr3 >= 1 << 32
        || bootstrap_cr3 & 4095 != 0
        || !low_canonical(entry)
        || (physical..physical + PAGE as u64).contains(&entry)
    {
        return Err(Error::Address);
    }
    if efer & !0x901 != 0
        || efer & 0x100 == 0
        || super::ept::HostPagingPolicy::new(
            pat,
            super::platform_memory::PageCapabilities::from_host_cpuid(0, 0),
        )
        .is_err()
    {
        return Err(Error::MemoryType);
    }
    if slots.is_empty() || slots.len() > MAX_CPUS {
        return Err(Error::CpuRecords);
    }
    for (i, slot) in slots.iter().enumerate() {
        if slots[..i].iter().any(|other| other.apic_id == slot.apic_id) {
            return Err(Error::CpuRecords);
        }
        if !low_canonical(slot.stack)
            || slot.stack & 15 != 8
            || !low_canonical(slot.argument)
            || !low_canonical(slot.host_cr3)
            || slot.host_cr3 & 4095 != 0
        {
            return Err(Error::Address);
        }
    }
    // Capture symbol addresses only; bounds precede the sole memory access.
    let (start, protected, long, probe, end) = (
        &raw const thin_hv_ap_boot_start as usize,
        &raw const thin_hv_ap_boot_protected as usize,
        &raw const thin_hv_ap_boot_long as usize,
        &raw const thin_hv_ap_boot_probe as usize,
        &raw const thin_hv_ap_boot_end as usize,
    );
    if !(start < protected && protected < long && long < probe && probe < end)
        || end - start > CONFIG
        || SLOTS + slots.len() * SLOT_BYTES > GDT
    {
        return Err(Error::CodeLayout);
    }
    // SAFETY: the ordered symbols above bound the immutable linked text. It is
    // disjoint from the caller's exclusive writable page and never mutated here.
    let code = unsafe { core::slice::from_raw_parts(start as *const u8, end - start) };
    output.fill(0);
    output[..code.len()].copy_from_slice(code);
    output[GDT_POINTER..GDT_POINTER + 2].copy_from_slice(&31_u16.to_le_bytes());
    output[GDT_POINTER + 2..GDT_POINTER + 6]
        .copy_from_slice(&((physical + GDT as u64) as u32).to_le_bytes());
    for (offset, target, selector) in [
        (PROTECTED_POINTER, protected, 8_u16),
        (LONG_POINTER, long, 24_u16),
    ] {
        output[offset..offset + 4]
            .copy_from_slice(&((physical + (target - start) as u64) as u32).to_le_bytes());
        output[offset + 4..offset + 6].copy_from_slice(&selector.to_le_bytes());
    }
    output[ROOT..ROOT + 4].copy_from_slice(&(bootstrap_cr3 as u32).to_le_bytes());
    for (offset, value) in [(PAT, pat), (EFER, efer), (ENTRY, entry)] {
        output[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    output[COUNT..COUNT + 4].copy_from_slice(&(slots.len() as u32).to_le_bytes());
    for (i, slot) in slots.iter().enumerate() {
        let offset = SLOTS + i * SLOT_BYTES;
        output[offset..offset + 4].copy_from_slice(&slot.apic_id.to_le_bytes());
        for (field, value) in [(8, slot.stack), (16, slot.argument), (24, slot.host_cr3)] {
            output[offset + field..offset + field + 8].copy_from_slice(&value.to_le_bytes());
        }
    }
    for (i, descriptor) in [
        0_u64,
        0x00cf_9b00_0000_ffff,
        0x00cf_9300_0000_ffff,
        0x00af_9b00_0000_ffff,
    ]
    .into_iter()
    .enumerate()
    {
        let offset = GDT + i * 8;
        output[offset..offset + 8].copy_from_slice(&descriptor.to_le_bytes());
    }
    Ok((probe - start) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_layout_is_bounded_and_rejects_partial_handoffs() {
        let mut output = [0xa5; PAGE];
        let slot = Slot {
            apic_id: 0x1234,
            stack: 0x1fff8,
            argument: 0x3000,
            host_cr3: 0x4000,
        };
        let build = |output: &mut [u8; PAGE], slots: &[Slot]| {
            prepare(
                output,
                0x9000,
                0xa000,
                0x0007_0406_0007_0406,
                0x900,
                0x200000,
                slots,
            )
        };
        let probe = build(&mut output, &[slot]).unwrap();
        assert!(usize::from(probe) < CONFIG);
        assert_eq!(output[0], 0xfa); // CLI, before any firmware-state assumptions.
        assert_eq!(&output[SLOTS..SLOTS + 4], &0x1234_u32.to_le_bytes());
        assert!(
            output[SLOTS + SLOT_BYTES..GDT]
                .iter()
                .all(|&byte| byte == 0)
        );
        let valid = output;
        for bad in [
            Slot {
                stack: 0x20000,
                ..slot
            },
            Slot {
                host_cr3: 0,
                ..slot
            },
            Slot {
                host_cr3: 0x4001,
                ..slot
            },
            Slot {
                argument: 1 << 47,
                ..slot
            },
        ] {
            assert_eq!(build(&mut output, &[bad]), Err(Error::Address));
            assert_eq!(output, valid);
        }
        for slots in [&[][..], &[slot, slot][..], &[slot; MAX_CPUS + 1][..]] {
            assert_eq!(build(&mut output, slots), Err(Error::CpuRecords));
            assert_eq!(output, valid);
        }
    }
}
