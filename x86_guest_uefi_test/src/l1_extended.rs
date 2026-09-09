//! Native live-register checks with no Rust between seeding and capture.
//! The caller's entire tested state is restored before returning to its ABI.

use super::Result;
use super::equal;
use core::arch::asm;

fn cr2() -> u64 {
    let value;
    // SAFETY: all probe callers run at CPL0; reading CR2 has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

#[repr(C, align(64))]
pub(super) struct Image {
    fx: [u8; 512],
    ymm: [u8; 512],
}

impl Image {
    pub(super) const fn zeroed() -> Self {
        Self {
            fx: [0; 512],
            ymm: [0; 512],
        }
    }
}

/// Operation numbers are private to the naked probe below.
#[derive(Clone, Copy)]
pub(super) enum Operation {
    Cpuid,
    CpuidD,
    Xsetbv(u64),
    Launch,
    Resume,
    Interrupt,
}

impl Operation {
    fn operands(self) -> (u64, u64) {
        match self {
            Self::Cpuid => (0, 0),
            Self::CpuidD => (1, 0),
            Self::Xsetbv(value) => (2, value),
            Self::Launch => (3, 0),
            Self::Resume => (4, 0),
            Self::Interrupt => (5, 0),
        }
    }
}

pub(super) fn seed() -> Image {
    let mut image = Image::zeroed();
    image.fx[0..2].copy_from_slice(&0x027f_u16.to_le_bytes());
    image.fx[4] = 0xff; // All eight x87 slots contain finite, nonzero values.
    image.fx[24..28].copy_from_slice(&0x5f80_u32.to_le_bytes());
    for register in 0..8 {
        let offset = 32 + register * 16;
        image.fx[offset..offset + 8]
            .copy_from_slice(&(0x8000_0000_0000_0000_u64 | (register as u64 + 1)).to_le_bytes());
        image.fx[offset + 8..offset + 10].copy_from_slice(&0x3fff_u16.to_le_bytes());
    }
    for register in 0..16 {
        for byte in 0..32 {
            let value = (register * 37 + byte * 11 + 1) as u8;
            image.ymm[register * 32 + byte] = value;
            if byte < 16 {
                image.fx[160 + register * 16 + byte] = value;
            }
        }
    }
    image
}

/// # Safety
/// CPL0, CR0.EM/TS=0, CR4.OSFXSR=1 and FXSR/SSE2 are required. XSETBV must be
/// a valid XCR0 write with OSXSAVE set. Launch/resume must be guaranteed to fail
/// before loading guest state in an owned VMCS (or with no current VMCS).
/// If avx is true, CPUID.AVX and XCR0[2:1]=11 were checked and no interrupt may
/// use temporary guest register state. All buffers are private live stack data.
/// Interrupt requires IF=0 and a live firmware timer/IDT: it enables IF only for
/// STI;HLT and clears it before capture. The QEMU runner bounds missing wakeups;
/// no firmware table, APIC route or timer configuration is changed.
pub(super) unsafe fn check(operation: Operation, avx: bool) -> Result<u64> {
    let before = seed();
    let mut after = Image::zeroed();
    let mut saved = Image::zeroed();
    let (operation, value) = operation.operands();
    let original_cr2 = cr2();
    // SAFETY: the caller supplies the CPU/instruction prerequisites. These
    // distinct 64-byte-aligned stack images live through the naked call, which
    // restores the caller's FP state without calling Rust or touching CR2.
    let flags = unsafe {
        probe(
            &before,
            &mut after,
            &mut saved,
            operation,
            value,
            u64::from(avx),
        )
    };
    equal("l1-extended-cr2", cr2(), original_cr2)?;
    verify(&before, &after, avx)?;
    Ok((flags & 1) | ((flags >> 5) & 2))
}

pub(super) fn verify(before: &Image, after: &Image, avx: bool) -> Result<()> {
    // Compare defined state only: byte 5, register padding and MXCSR_MASK are
    // reserved/implementation metadata, not restored architectural registers.
    for byte in (0..5).chain(6..28).chain(160..416) {
        equal(
            "l1-extended-fx",
            u64::from(after.fx[byte]),
            u64::from(before.fx[byte]),
        )?;
    }
    for register in 0..8 {
        for byte in 0..10 {
            let offset = 32 + register * 16 + byte;
            equal(
                "l1-extended-x87",
                u64::from(after.fx[offset]),
                u64::from(before.fx[offset]),
            )?;
        }
    }
    if avx {
        for byte in 0..512 {
            equal(
                "l1-extended-ymm",
                u64::from(after.ymm[byte]),
                u64::from(before.ymm[byte]),
            )?;
        }
    }
    Ok(())
}

// SAFETY: only check calls this at CPL0 with three disjoint aligned live images
// and validated instruction prerequisites. SysV arguments use RDI/RSI/RDX,
// RCX/R8/R9; RBX is saved, and R10 retains the third buffer across CPUID. AVX
// instructions are reachable only with the checked sixth argument set. No Rust
// executes while sentinels are live; all paths restore the caller's registers.
#[unsafe(naked)]
unsafe extern "sysv64" fn probe(
    _before: *const Image,
    _after: *mut Image,
    _saved: *mut Image,
    _operation: u64,
    _value: u64,
    _avx: u64,
) -> u64 {
    core::arch::naked_asm!(
        "push rbx",
        "mov r10, rdx",
        "mov r11, rcx",
        "fxsave64 [r10]",
        "test r9, r9",
        "jz 2f",
        "vmovdqu [r10 + 512], ymm0",
        "vmovdqu [r10 + 544], ymm1",
        "vmovdqu [r10 + 576], ymm2",
        "vmovdqu [r10 + 608], ymm3",
        "vmovdqu [r10 + 640], ymm4",
        "vmovdqu [r10 + 672], ymm5",
        "vmovdqu [r10 + 704], ymm6",
        "vmovdqu [r10 + 736], ymm7",
        "vmovdqu [r10 + 768], ymm8",
        "vmovdqu [r10 + 800], ymm9",
        "vmovdqu [r10 + 832], ymm10",
        "vmovdqu [r10 + 864], ymm11",
        "vmovdqu [r10 + 896], ymm12",
        "vmovdqu [r10 + 928], ymm13",
        "vmovdqu [r10 + 960], ymm14",
        "vmovdqu [r10 + 992], ymm15",
        "2:",
        "fxrstor64 [rdi]",
        "test r9, r9",
        "jz 3f",
        "vmovdqu ymm0, [rdi + 512]",
        "vmovdqu ymm1, [rdi + 544]",
        "vmovdqu ymm2, [rdi + 576]",
        "vmovdqu ymm3, [rdi + 608]",
        "vmovdqu ymm4, [rdi + 640]",
        "vmovdqu ymm5, [rdi + 672]",
        "vmovdqu ymm6, [rdi + 704]",
        "vmovdqu ymm7, [rdi + 736]",
        "vmovdqu ymm8, [rdi + 768]",
        "vmovdqu ymm9, [rdi + 800]",
        "vmovdqu ymm10, [rdi + 832]",
        "vmovdqu ymm11, [rdi + 864]",
        "vmovdqu ymm12, [rdi + 896]",
        "vmovdqu ymm13, [rdi + 928]",
        "vmovdqu ymm14, [rdi + 960]",
        "vmovdqu ymm15, [rdi + 992]",
        "3:",
        "cmp r11, 2",
        "je 5f",
        "cmp r11, 3",
        "je 6f",
        "cmp r11, 4",
        "je 7f",
        "cmp r11, 5",
        "je 11f",
        "mov eax, 1",
        "test r11, r11",
        "jz 4f",
        "mov eax, 0xd",
        "4:",
        "xor ecx, ecx",
        "cpuid",
        "jmp 8f",
        "5:",
        "mov rax, r8",
        "mov rdx, r8",
        "shr rdx, 32",
        "xor ecx, ecx",
        "xsetbv",
        "jmp 8f",
        "6:",
        "vmlaunch",
        "jmp 8f",
        "7:",
        "vmresume",
        "8:",
        "pushfq",
        "pop rax",
        "fxsave64 [rsi]",
        "test r9, r9",
        "jz 9f",
        "vmovdqu [rsi + 512], ymm0",
        "vmovdqu [rsi + 544], ymm1",
        "vmovdqu [rsi + 576], ymm2",
        "vmovdqu [rsi + 608], ymm3",
        "vmovdqu [rsi + 640], ymm4",
        "vmovdqu [rsi + 672], ymm5",
        "vmovdqu [rsi + 704], ymm6",
        "vmovdqu [rsi + 736], ymm7",
        "vmovdqu [rsi + 768], ymm8",
        "vmovdqu [rsi + 800], ymm9",
        "vmovdqu [rsi + 832], ymm10",
        "vmovdqu [rsi + 864], ymm11",
        "vmovdqu [rsi + 896], ymm12",
        "vmovdqu [rsi + 928], ymm13",
        "vmovdqu [rsi + 960], ymm14",
        "vmovdqu [rsi + 992], ymm15",
        "9:",
        "fxrstor64 [r10]",
        "test r9, r9",
        "jz 10f",
        "vmovdqu ymm0, [r10 + 512]",
        "vmovdqu ymm1, [r10 + 544]",
        "vmovdqu ymm2, [r10 + 576]",
        "vmovdqu ymm3, [r10 + 608]",
        "vmovdqu ymm4, [r10 + 640]",
        "vmovdqu ymm5, [r10 + 672]",
        "vmovdqu ymm6, [r10 + 704]",
        "vmovdqu ymm7, [r10 + 736]",
        "vmovdqu ymm8, [r10 + 768]",
        "vmovdqu ymm9, [r10 + 800]",
        "vmovdqu ymm10, [r10 + 832]",
        "vmovdqu ymm11, [r10 + 864]",
        "vmovdqu ymm12, [r10 + 896]",
        "vmovdqu ymm13, [r10 + 928]",
        "vmovdqu ymm14, [r10 + 960]",
        "vmovdqu ymm15, [r10 + 992]",
        "10:",
        "pop rbx",
        "ret",
        "11:",
        "sti",
        "hlt",
        "cli",
        "jmp 8b",
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn defined_seed_regions_match_legacy_and_avx_layouts() {
        let image = super::seed();
        assert_eq!(core::mem::align_of::<super::Image>(), 64);
        assert_eq!(core::mem::offset_of!(super::Image, ymm), 512);
        assert_eq!(core::mem::size_of::<super::Image>(), 1024);
        for register in 0..16 {
            assert_eq!(
                &image.fx[160 + register * 16..176 + register * 16],
                &image.ymm[register * 32..register * 32 + 16],
            );
        }
        assert!(image.ymm.iter().any(|byte| *byte != 0));
    }
}
