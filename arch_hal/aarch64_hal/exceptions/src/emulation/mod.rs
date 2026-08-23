use core::mem::size_of;

use crate::memory_hook::MmioHandler;
use crate::registers::DataFaultStatusCade;
use crate::registers::ESR_EL2;
use crate::registers::InstructionRegisterSize;
use crate::registers::SyndromeAccessSize;
use crate::registers::WriteNotRead;
use crate::synchronous_handler::DataAbortInfo;
use cpu::Registers;
#[cfg(not(test))]
use cpu::get_elr_el2;
use cpu::get_sp_el1;
use cpu::read_guest_insn_u32_at_el1_pc;
#[cfg(not(test))]
use cpu::set_elr_el2;
use cpu::set_sp_el1;
use cpu::va_to_ipa_el2_read;
use cpu::va_to_ipa_el2_write;

#[cfg(test)]
use core::sync::atomic::AtomicU64;
#[cfg(test)]
use core::sync::atomic::Ordering;

pub mod load;
pub mod store;

/// Stage-2 page size in bytes. Must match the configured stage-2 granule.
/// TODO: plumb this from the paging configuration rather than hard-coding.
const STAGE2_PAGE_SIZE: u64 = common::mem::PAGE_SIZE_4K_U64;
const MIN_GUEST_PAGE_SIZE: u64 = common::mem::PAGE_SIZE_4K_U64;

#[cfg(test)]
static TEST_ELR: AtomicU64 = AtomicU64::new(0);

#[inline]
fn read_elr_el2() -> u64 {
    #[cfg(test)]
    {
        return TEST_ELR.load(Ordering::Relaxed);
    }
    #[cfg(not(test))]
    {
        get_elr_el2()
    }
}

#[inline]
fn write_elr_el2(val: u64) {
    #[cfg(test)]
    {
        TEST_ELR.store(val, Ordering::Relaxed);
        return;
    }
    #[cfg(not(test))]
    {
        set_elr_el2(val);
    }
}

#[derive(Copy, Clone, Debug)]
pub enum EmulationOutcome {
    Handled,
    NotHandled,
}

#[derive(Copy, Clone, Debug)]
pub enum MmioDecoded {
    Prefetch {
        insn: u32,
    },
    Single {
        desc: SingleDesc,
        fault_va: u64,
        ipa: u64,
        split: Option<SplitPlan>,
    },
    Pair {
        desc: PairDesc,
        fault_va: u64,
        ipa0: u64,
        ipa1: u64,
        split0: Option<SplitPlan>,
        split1: Option<SplitPlan>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Writeback {
    None,
    Pre,
    Post,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SingleAddrMode {
    UnsignedOffset,
    PreIndex,
    PostIndex,
    Unscaled,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SplitSegment {
    pub ipa: u64,
    pub size: u8,
    pub byte_offset: u8,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SplitPlan {
    segs: [SplitSegment; 8],
    len: u8,
}

impl SplitPlan {
    pub const EMPTY_SEGMENT: SplitSegment = SplitSegment {
        ipa: 0,
        size: 0,
        byte_offset: 0,
    };

    pub const fn new() -> Self {
        Self {
            segs: [Self::EMPTY_SEGMENT; 8],
            len: 0,
        }
    }

    pub fn segments(&self) -> &[SplitSegment] {
        &self.segs[..self.len as usize]
    }
}

#[derive(Copy, Clone, Debug)]
pub struct SingleDesc {
    pub rn: u8,
    pub rt: u8,
    pub rt_size: InstructionRegisterSize,
    pub size: u8,
    pub sign_extend: bool,
    pub offset: i64,
    pub addr_mode: SingleAddrMode,
    pub is_store: bool,
}

#[derive(Copy, Clone, Debug)]
pub struct PairDesc {
    pub rn: u8,
    pub rt: u8,
    pub rt2: u8,
    pub rt_size: InstructionRegisterSize,
    pub size: u8,
    pub offset: i64,
    pub writeback: Writeback,
    pub is_store: bool,
}

#[derive(Copy, Clone, Debug)]
enum DecodedAccess {
    Single(SingleDesc),
    Pair(PairDesc),
    Prefetch,
}

pub(crate) fn base_writeback(regs: &[u64; 32], rn: u8) -> u64 {
    if rn == 31 {
        get_sp_el1()
    } else {
        regs[rn as usize]
    }
}

/// Applies base writeback for single-register pre/post-index addressing.
fn apply_writeback(regs: &mut [u64; 32], rn: u8, offset: i64, mode: SingleAddrMode) {
    if matches!(mode, SingleAddrMode::PostIndex | SingleAddrMode::PreIndex) {
        let base = base_writeback(regs, rn).wrapping_add(offset as u64);
        write_back_base(regs, rn, base);
    }
}

/// Applies base writeback for pair pre/post-index addressing.
fn apply_pair_writeback(regs: &mut [u64; 32], rn: u8, offset: i64, mode: Writeback) {
    if matches!(mode, Writeback::Post | Writeback::Pre) {
        let base = base_writeback(regs, rn).wrapping_add(offset as u64);
        write_back_base(regs, rn, base);
    }
}

/// Writes a decoded base value to SP_EL1 or its general-purpose register.
fn write_back_base(regs: &mut [u64; 32], rn: u8, val: u64) {
    if rn == 31 {
        set_sp_el1(val);
    } else {
        regs[rn as usize] = val;
    }
}

pub(crate) fn crosses_stage2_page(ipa: u64, size: u8) -> bool {
    let offset_in_page = ipa & (STAGE2_PAGE_SIZE - 1);
    offset_in_page + size as u64 > STAGE2_PAGE_SIZE
}

pub(crate) fn crosses_guest_min_page(va: u64, size: u8) -> bool {
    // Stage-1 page size is unknown; 4KiB is the conservative lower bound. If an access crosses
    // this boundary, the VA->IPA mapping may be non-contiguous, so we reject emulation.
    let off = va & (MIN_GUEST_PAGE_SIZE - 1);
    off + size as u64 > MIN_GUEST_PAGE_SIZE
}

pub(crate) fn plan_split_access(base_ipa: u64, total_size: u8) -> Option<SplitPlan> {
    plan_split_with_page(base_ipa, total_size, STAGE2_PAGE_SIZE)
}

fn plan_split_with_page(base_ipa: u64, total_size: u8, page_size: u64) -> Option<SplitPlan> {
    if total_size == 0 {
        return None;
    }

    let mut plan = SplitPlan::new();
    let mut produced = 0u8;
    while produced < total_size {
        let ipa = base_ipa.wrapping_add(produced as u64);
        let page_remaining = page_size - (ipa & (page_size - 1));
        let mut chosen: Option<u8> = None;
        for candidate in [8u8, 4, 2, 1] {
            if candidate > (total_size - produced) {
                continue;
            }
            if ipa % candidate as u64 != 0 {
                continue;
            }
            if (candidate as u64) > page_remaining {
                continue;
            }
            chosen = Some(candidate);
            break;
        }
        let seg_size = chosen?;
        if (plan.len as usize) >= plan.segs.len() {
            return None;
        }
        plan.segs[plan.len as usize] = SplitSegment {
            ipa,
            size: seg_size,
            byte_offset: produced,
        };
        plan.len += 1;
        produced += seg_size;
    }

    Some(plan)
}

pub(crate) fn can_handle_plan(handler: &dyn MmioHandler, plan: &SplitPlan, is_write: bool) -> bool {
    plan.segments()
        .iter()
        .all(|seg| handler.can_split_subaccess(seg.ipa, seg.size, is_write))
}

#[cfg(feature = "testapi")]
pub fn test_plan_split(base_ipa: u64, total_size: u8, page_size: u64) -> Option<SplitPlan> {
    plan_split_with_page(base_ipa, total_size, page_size)
}

pub fn decode_mmio(regs: &Registers, info: &DataAbortInfo) -> Option<MmioDecoded> {
    let esr = &info.esr_el2;
    if esr.get(ESR_EL2::fnv) != 0 {
        return None;
    }
    if esr.get(ESR_EL2::s1ptw) != 0 {
        return None;
    }
    if !is_emulatable_stage2_dfsc(esr) {
        return None;
    }
    let insn = read_guest_insn_u32_at_el1_pc(read_elr_el2())?;
    decode_mmio_with_insn(regs, info, insn, translate_ipa)
}

fn decode_mmio_with_insn<R>(
    regs: &Registers,
    info: &DataAbortInfo,
    insn: u32,
    resolver: R,
) -> Option<MmioDecoded>
where
    R: Fn(u64, bool) -> Option<u64>,
{
    let esr = &info.esr_el2;
    if esr.get(ESR_EL2::fnv) != 0 {
        return None;
    }
    if !is_emulatable_stage2_dfsc(esr) {
        return None;
    }

    let Some(decoded) = decode_access(insn) else {
        return None;
    };

    let fault_va = info.far_el2;
    let fault_ipa = info.fault_ipa;

    let iss_is_write = if esr.get(ESR_EL2::isv) != 0 {
        esr.get_enum::<_, WriteNotRead>(ESR_EL2::wnr)
            .map(|v| matches!(v, WriteNotRead::WritingMemoryAbort))
    } else {
        None
    };

    match decoded {
        DecodedAccess::Prefetch => Some(MmioDecoded::Prefetch { insn }),
        DecodedAccess::Single(desc) => {
            let insn_is_write = desc.is_store;
            if let Some(iss_write) = iss_is_write {
                if iss_write != insn_is_write {
                    return None;
                }
            }
            if !sas_matches(esr, desc.size) {
                return None;
            }

            let regs_arr = regs.gprs();
            let base = base_writeback(regs_arr, desc.rn);
            let eff = single_effective(base, &desc);
            if desc.size > 1 && crosses_guest_min_page(eff, desc.size) {
                return None;
            }

            let first_ipa = resolve_ipa_with_resolver(
                fault_va,
                insn_is_write,
                esr,
                fault_ipa,
                true,
                fault_va,
                &resolver,
            )?;

            let ipa = if eff == fault_va {
                first_ipa
            } else {
                resolve_ipa_with_resolver(
                    eff,
                    insn_is_write,
                    esr,
                    fault_ipa,
                    false,
                    fault_va,
                    &resolver,
                )?
            };

            let split = split_plan_for_access(ipa, desc.size)?;

            Some(MmioDecoded::Single {
                desc,
                fault_va,
                ipa,
                split,
            })
        }
        DecodedAccess::Pair(desc) => {
            let insn_is_write = desc.is_store;
            if let Some(iss_write) = iss_is_write {
                if iss_write != insn_is_write {
                    return None;
                }
            }
            if !sas_matches(esr, desc.size) {
                return None;
            }

            let regs_arr = regs.gprs();
            let base = base_writeback(regs_arr, desc.rn);
            let (addr0, addr1) = pair_effective(base, &desc);
            if desc.size > 1 && crosses_guest_min_page(addr0, desc.size) {
                return None;
            }
            if desc.size > 1 && crosses_guest_min_page(addr1, desc.size) {
                return None;
            }

            let first_ipa = resolve_ipa_with_resolver(
                fault_va,
                insn_is_write,
                esr,
                fault_ipa,
                true,
                fault_va,
                &resolver,
            )?;
            let ipa0 = if addr0 == fault_va {
                first_ipa
            } else {
                resolve_ipa_with_resolver(
                    addr0,
                    insn_is_write,
                    esr,
                    fault_ipa,
                    addr0 == fault_va,
                    fault_va,
                    &resolver,
                )?
            };
            let ipa1 = if addr1 == fault_va {
                first_ipa
            } else {
                resolve_ipa_with_resolver(
                    addr1,
                    insn_is_write,
                    esr,
                    fault_ipa,
                    addr1 == fault_va,
                    fault_va,
                    &resolver,
                )?
            };

            let split0 = split_plan_for_access(ipa0, desc.size)?;
            let split1 = split_plan_for_access(ipa1, desc.size)?;

            Some(MmioDecoded::Pair {
                desc,
                fault_va,
                ipa0,
                ipa1,
                split0,
                split1,
            })
        }
    }
}

fn split_plan_for_access(ipa: u64, size: u8) -> Option<Option<SplitPlan>> {
    if size > 1 && (ipa % size as u64 != 0 || crosses_stage2_page(ipa, size)) {
        Some(Some(plan_split_access(ipa, size)?))
    } else {
        Some(None)
    }
}

pub fn execute_mmio(
    regs: &mut Registers,
    info: &DataAbortInfo,
    handler: &dyn MmioHandler,
    decoded: &MmioDecoded,
) -> EmulationOutcome {
    let regs_arr = regs.gprs_mut();
    let result = match decoded {
        MmioDecoded::Prefetch { .. } => EmulationOutcome::Handled,
        MmioDecoded::Single {
            desc, ipa, split, ..
        } => {
            if desc.is_store {
                store::emulate_single(regs_arr, desc, *ipa, split.as_ref(), handler)
            } else {
                load::emulate_single(regs_arr, desc, *ipa, split.as_ref(), handler)
            }
        }
        MmioDecoded::Pair {
            desc,
            ipa0,
            ipa1,
            split0,
            split1,
            ..
        } => {
            if desc.is_store {
                store::emulate_pair(
                    regs_arr,
                    desc,
                    *ipa0,
                    *ipa1,
                    split0.as_ref(),
                    split1.as_ref(),
                    handler,
                )
            } else {
                load::emulate_pair(
                    regs_arr,
                    desc,
                    *ipa0,
                    *ipa1,
                    split0.as_ref(),
                    split1.as_ref(),
                    handler,
                )
            }
        }
    };

    if let EmulationOutcome::Handled = result {
        let inc = size_of::<u32>() as u64;
        write_elr_el2(read_elr_el2().wrapping_add(inc));
    }
    let _ = info;
    result
}

pub fn try_emulate_mmio(
    regs: &mut Registers,
    info: &DataAbortInfo,
    handler: &dyn MmioHandler,
) -> EmulationOutcome {
    match decode_mmio(&*regs, info) {
        Some(decoded) => execute_mmio(regs, info, handler, &decoded),
        None => EmulationOutcome::NotHandled,
    }
}

fn is_emulatable_stage2_dfsc(esr: &ESR_EL2) -> bool {
    // SAFETY: We only emulate faults that are expected to be generated by stage-2 trapping.
    if esr.get(ESR_EL2::s1ptw) != 0 {
        return false;
    }
    matches!(
        esr.get_enum::<_, DataFaultStatusCade>(ESR_EL2::dfsc),
        Some(
            DataFaultStatusCade::AddressSizeLevel0
                | DataFaultStatusCade::AddressSizeLevel1
                | DataFaultStatusCade::AddressSizeLevel2
                | DataFaultStatusCade::AddressSizeLevel3
                | DataFaultStatusCade::TranslationLevel0
                | DataFaultStatusCade::TranslationLevel1
                | DataFaultStatusCade::TranslationLevel2
                | DataFaultStatusCade::TranslationLevel3
                | DataFaultStatusCade::AccessFlagLevel0
                | DataFaultStatusCade::AccessFlagLevel1
                | DataFaultStatusCade::AccessFlagLevel2
                | DataFaultStatusCade::AccessFlagLevel3
                | DataFaultStatusCade::PermissionLevel0
                | DataFaultStatusCade::PermissionLevel1
                | DataFaultStatusCade::PermissionLevel2
                | DataFaultStatusCade::PermissionLevel3,
        )
    )
}

fn hpfar_valid(esr: &ESR_EL2) -> bool {
    if esr.get(ESR_EL2::fnv) != 0 {
        return false;
    }
    hpfar_el2_written_for_abort(esr)
}

/// Returns true when HPFAR_EL2 is architecturally written for this stage-2 fault.
pub(crate) fn hpfar_el2_written_for_abort(esr: &ESR_EL2) -> bool {
    if esr.get(ESR_EL2::s1ptw) != 0 {
        return false;
    }
    matches!(
        esr.get_enum::<_, DataFaultStatusCade>(ESR_EL2::dfsc),
        Some(
            DataFaultStatusCade::AddressSizeLevel0
                | DataFaultStatusCade::AddressSizeLevel1
                | DataFaultStatusCade::AddressSizeLevel2
                | DataFaultStatusCade::AddressSizeLevel3
                | DataFaultStatusCade::TranslationLevel0
                | DataFaultStatusCade::TranslationLevel1
                | DataFaultStatusCade::TranslationLevel2
                | DataFaultStatusCade::TranslationLevel3
                | DataFaultStatusCade::AccessFlagLevel0
                | DataFaultStatusCade::AccessFlagLevel1
                | DataFaultStatusCade::AccessFlagLevel2
                | DataFaultStatusCade::AccessFlagLevel3,
        )
    )
}

fn resolve_ipa_with_resolver<R>(
    va: u64,
    is_write: bool,
    esr: &ESR_EL2,
    fault_ipa: Option<u64>,
    allow_hpfar: bool,
    fault_va: u64,
    resolver: &R,
) -> Option<u64>
where
    R: Fn(u64, bool) -> Option<u64>,
{
    if allow_hpfar && hpfar_valid(esr) && va == fault_va {
        if let Some(ipa) = fault_ipa {
            return Some(ipa);
        }
    }
    resolver(va, is_write)
}

fn translate_ipa(va: u64, is_write: bool) -> Option<u64> {
    if is_write {
        va_to_ipa_el2_write(va)
    } else {
        va_to_ipa_el2_read(va)
    }
}

fn sas_matches(esr: &ESR_EL2, expected_size: u8) -> bool {
    if esr.get(ESR_EL2::isv) == 0 {
        return true;
    }
    let sas = match esr.get_enum::<_, SyndromeAccessSize>(ESR_EL2::sas) {
        Some(v) => v,
        None => return false,
    };
    let esr_size = match sas {
        SyndromeAccessSize::Byte => 1,
        SyndromeAccessSize::HalfWord => 2,
        SyndromeAccessSize::Word => 4,
        SyndromeAccessSize::DoubleWord => 8,
    };
    esr_size == expected_size
}

fn decode_access(insn: u32) -> Option<DecodedAccess> {
    if is_prfm(insn) {
        return Some(DecodedAccess::Prefetch);
    }
    if let Some(desc) = decode_single(insn) {
        return Some(DecodedAccess::Single(desc));
    }
    if let Some(desc) = decode_pair(insn) {
        return Some(DecodedAccess::Pair(desc));
    }
    None
}

fn decode_single(insn: u32) -> Option<SingleDesc> {
    const LS_MASK: u32 = 0x3b00_0000;
    match insn & LS_MASK {
        0x3800_0000 | 0x3900_0000 => {}
        _ => return None,
    }

    if is_prfm(insn) {
        return None;
    }

    let size_bits = (insn >> 30) & 0x3;
    let size = 1u8 << size_bits;

    let opc = (insn >> 22) & 0x3;
    let (is_store, sign_extend, rt_size) = if (opc & 0b10) == 0 {
        let is_store = (opc & 0b1) == 0;
        let rt_size = if size_bits == 3 {
            InstructionRegisterSize::Instruction64bit
        } else {
            InstructionRegisterSize::Instruction32bit
        };
        (is_store, false, rt_size)
    } else {
        if size_bits == 3 {
            return None;
        }
        if size_bits == 2 && (opc & 0b1) != 0 {
            return None;
        }
        let rt_size = if (opc & 0b1) != 0 {
            InstructionRegisterSize::Instruction32bit
        } else {
            InstructionRegisterSize::Instruction64bit
        };
        (false, true, rt_size)
    };

    let rn = ((insn >> 5) & 0x1f) as u8;
    let rt = (insn & 0x1f) as u8;

    let mode_bits = (insn >> 23) & 0x3;
    let (addr_mode, offset) = match mode_bits {
        0b00 => (
            SingleAddrMode::Unscaled,
            sign_extend_9(((insn >> 12) & 0x1ff) as u32) as i64,
        ),
        0b01 => (
            SingleAddrMode::PostIndex,
            sign_extend_9(((insn >> 12) & 0x1ff) as u32) as i64,
        ),
        0b10 => {
            let imm12 = (insn >> 10) & 0xfff;
            (
                SingleAddrMode::UnsignedOffset,
                (imm12 as i64) * (size as i64),
            )
        }
        0b11 => (
            SingleAddrMode::PreIndex,
            sign_extend_9(((insn >> 12) & 0x1ff) as u32) as i64,
        ),
        _ => unreachable!(),
    };

    Some(SingleDesc {
        rn,
        rt,
        rt_size,
        size,
        sign_extend,
        offset,
        addr_mode,
        is_store,
    })
}

fn decode_pair(insn: u32) -> Option<PairDesc> {
    // opc[31:30], 101[29:27], V[26], 0[25], index[24:23], L[22], imm7[21:15], Rt2[14:10], Rn[9:5], Rt[4:0]
    if ((insn >> 27) & 0x7) != 0b101 {
        return None;
    }
    if ((insn >> 26) & 0x1) != 0 {
        // Vector register form, not handled here.
        return None;
    }
    if ((insn >> 25) & 0x1) != 0 {
        return None;
    }

    let opc = (insn >> 30) & 0x3;
    let (size, rt_size) = match opc {
        0b00 => (4u8, InstructionRegisterSize::Instruction32bit),
        0b10 => (8u8, InstructionRegisterSize::Instruction64bit),
        // Reject LDPSW (opc == 01) and unallocated encodings (opc == 11).
        _ => return None,
    };

    let is_store = ((insn >> 22) & 0x1) == 0;

    let writeback = match (insn >> 23) & 0x3 {
        0b01 => Writeback::Post,
        0b10 => Writeback::None,
        0b11 => Writeback::Pre,
        // LDNP/STNP (00) are treated as unhandled to avoid mis-decoding.
        0b00 => return None,
        _ => return None,
    };

    let imm7 = ((insn >> 15) & 0x7f) as u32;
    let offset = (sign_extend_7(imm7) as i64) * (size as i64);

    Some(PairDesc {
        rn: ((insn >> 5) & 0x1f) as u8,
        rt: (insn & 0x1f) as u8,
        rt2: ((insn >> 10) & 0x1f) as u8,
        rt_size,
        size,
        offset,
        writeback,
        is_store,
    })
}

/// Test-only helper for exercising pair decode logic from integration tests.
#[cfg(feature = "testapi")]
pub fn test_decode_pair(insn: u32) -> Option<PairDesc> {
    decode_pair(insn)
}

fn sign_extend_7(val: u32) -> i32 {
    ((val << 25) as i32) >> 25
}

pub(crate) fn is_prfm(insn: u32) -> bool {
    const PRFM_IMM_MASK: u32 = 0xffc0_0000;
    const PRFM_IMM: u32 = 0xf980_0000;

    const PRFUM_MASK: u32 = 0xffe0_0000;
    const PRFUM: u32 = 0xf880_0000;

    const PRFM_LIT_MASK: u32 = 0xff00_0000;
    const PRFM_LIT: u32 = 0xd800_0000;

    (insn & PRFM_IMM_MASK) == PRFM_IMM
        || (insn & PRFUM_MASK) == PRFUM
        || (insn & PRFM_LIT_MASK) == PRFM_LIT
}

fn sign_extend_9(val: u32) -> i32 {
    ((val << 23) as i32) >> 23
}

fn single_effective(base: u64, desc: &SingleDesc) -> u64 {
    match desc.addr_mode {
        SingleAddrMode::UnsignedOffset | SingleAddrMode::PreIndex | SingleAddrMode::Unscaled => {
            base.wrapping_add(desc.offset as u64)
        }
        SingleAddrMode::PostIndex => base,
    }
}

fn pair_effective(base: u64, desc: &PairDesc) -> (u64, u64) {
    let addr0 = match desc.writeback {
        Writeback::Post => base,
        Writeback::Pre | Writeback::None => base.wrapping_add(desc.offset as u64),
    };
    let addr1 = addr0.wrapping_add(desc.size as u64);
    (addr0, addr1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulation::load::emulate_pair as emulate_pair_load;
    use crate::emulation::load::emulate_single as emulate_single_load;
    use crate::emulation::store::emulate_single as emulate_single_store;
    use crate::memory_hook::AccessClass;
    use crate::memory_hook::MmioError;
    use crate::memory_hook::MmioHandler;
    use crate::memory_hook::SplitPolicy;
    use crate::registers::InstructionRegisterSize;
    use crate::registers::InstructionRegisterSize::Instruction32bit;
    use crate::registers::InstructionRegisterSize::Instruction64bit;
    use crate::registers::SyndromeAccessSize;
    use crate::registers::WriteNotRead;

    const TRANSLATION_RELATED_FAULTS: [DataFaultStatusCade; 12] = [
        DataFaultStatusCade::AddressSizeLevel0,
        DataFaultStatusCade::AddressSizeLevel1,
        DataFaultStatusCade::AddressSizeLevel2,
        DataFaultStatusCade::AddressSizeLevel3,
        DataFaultStatusCade::TranslationLevel0,
        DataFaultStatusCade::TranslationLevel1,
        DataFaultStatusCade::TranslationLevel2,
        DataFaultStatusCade::TranslationLevel3,
        DataFaultStatusCade::AccessFlagLevel0,
        DataFaultStatusCade::AccessFlagLevel1,
        DataFaultStatusCade::AccessFlagLevel2,
        DataFaultStatusCade::AccessFlagLevel3,
    ];

    const PERMISSION_FAULTS: [DataFaultStatusCade; 4] = [
        DataFaultStatusCade::PermissionLevel0,
        DataFaultStatusCade::PermissionLevel1,
        DataFaultStatusCade::PermissionLevel2,
        DataFaultStatusCade::PermissionLevel3,
    ];

    fn esr_with_dfsc(dfsc: DataFaultStatusCade, s1ptw: bool) -> ESR_EL2 {
        let mut raw = dfsc as u64;
        if s1ptw {
            raw |= 1 << 7;
        }
        ESR_EL2::from_bits(raw)
    }

    fn zero_regs() -> Registers {
        unsafe { core::mem::zeroed() }
    }

    fn info_with_esr(esr: ESR_EL2, far_el2: u64, fault_ipa: Option<u64>) -> DataAbortInfo {
        DataAbortInfo {
            esr_el2: esr,
            far_el2,
            fault_ipa,
            access: None,
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn is_emulatable_accepts_translation_related_stage2_faults() {
        for dfsc in TRANSLATION_RELATED_FAULTS {
            let esr = esr_with_dfsc(dfsc, false);
            assert!(
                is_emulatable_stage2_dfsc(&esr),
                "dfsc {:?} should be emulatable",
                dfsc
            );
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn is_emulatable_accepts_permission_faults() {
        for dfsc in PERMISSION_FAULTS {
            let esr = esr_with_dfsc(dfsc, false);
            assert!(
                is_emulatable_stage2_dfsc(&esr),
                "dfsc {:?} should be emulatable",
                dfsc
            );
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn is_emulatable_rejects_s1ptw_faults() {
        for dfsc in TRANSLATION_RELATED_FAULTS {
            let esr = esr_with_dfsc(dfsc, true);
            assert!(
                !is_emulatable_stage2_dfsc(&esr),
                "dfsc {:?} should not be emulatable when s1ptw is set",
                dfsc
            );
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn decode_mmio_rejects_fnv() {
        let esr = {
            let tmp = ESR_EL2::from_bits(DataFaultStatusCade::TranslationLevel0 as u64);
            tmp.set(ESR_EL2::fnv, 1);
            tmp
        };
        let regs = zero_regs();
        let info = info_with_esr(esr, 0x1000, Some(0x2000));
        assert!(decode_mmio(&regs, &info).is_none());
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn decode_mmio_rejects_s1ptw() {
        let esr = esr_with_dfsc(DataFaultStatusCade::TranslationLevel0, true);
        let regs = zero_regs();
        let info = info_with_esr(esr, 0x1000, Some(0x2000));
        assert!(decode_mmio(&regs, &info).is_none());
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn decode_mmio_decodes_ldr() {
        let esr = {
            let tmp = ESR_EL2::from_bits(DataFaultStatusCade::TranslationLevel0 as u64);
            tmp.set(ESR_EL2::isv, 1);
            tmp.set(ESR_EL2::sas, SyndromeAccessSize::DoubleWord.into());
            tmp.set(ESR_EL2::wnr, WriteNotRead::ReadingMemoryAbort.into());
            tmp
        };
        let mut regs = zero_regs();
        regs.gprs_mut()[1] = 0x1000;
        const LDR_X0_FROM_X1: u32 = 0xf940_0020;
        let info = info_with_esr(esr, 0x1000, Some(0x2000));
        let decoded =
            decode_mmio_with_insn(&regs, &info, LDR_X0_FROM_X1, |va, _| Some(va + 0x1000))
                .expect("decode should succeed");
        match decoded {
            MmioDecoded::Single {
                desc,
                fault_va,
                ipa,
                split,
            } => {
                assert_eq!(fault_va, 0x1000);
                assert_eq!(ipa, 0x2000);
                assert_eq!(desc.rn, 1);
                assert_eq!(desc.rt, 0);
                assert!(!desc.is_store);
                assert_eq!(desc.size, 8);
                assert!(split.is_none());
            }
            _ => panic!("unexpected decoded variant"),
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn crosses_guest_min_page_detects_boundary() {
        assert!(crosses_guest_min_page(MIN_GUEST_PAGE_SIZE - 1, 2));
        assert!(!crosses_guest_min_page(MIN_GUEST_PAGE_SIZE - 2, 2));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn decode_mmio_rejects_guest_min_page_crossing() {
        let esr = {
            let tmp = ESR_EL2::from_bits(DataFaultStatusCade::TranslationLevel0 as u64);
            tmp.set(ESR_EL2::isv, 1);
            tmp.set(ESR_EL2::sas, SyndromeAccessSize::Word.into());
            tmp.set(ESR_EL2::wnr, WriteNotRead::ReadingMemoryAbort.into());
            tmp
        };
        let mut regs = zero_regs();
        regs.gprs_mut()[1] = MIN_GUEST_PAGE_SIZE - 1;
        const LDR_W0_FROM_X1: u32 = 0xb940_0020;
        let info = info_with_esr(esr, MIN_GUEST_PAGE_SIZE - 1, Some(0x2000));
        let decoded =
            decode_mmio_with_insn(&regs, &info, LDR_W0_FROM_X1, |va, _| Some(va + 0x1000));
        assert!(decoded.is_none());
    }

    fn encode_pair(opc: u32, index: u32, l: u32, imm7: i32, rn: u32, rt: u32, rt2: u32) -> u32 {
        ((opc & 0x3) << 30)
            | (0b101 << 27)
            | (0 << 26)
            | (0 << 25)
            | ((index & 0x3) << 23)
            | ((l & 0x1) << 22)
            | (((imm7 as u32) & 0x7f) << 15)
            | ((rt2 & 0x1f) << 10)
            | ((rn & 0x1f) << 5)
            | (rt & 0x1f)
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn decode_pair_stp_offset_64() {
        let insn = encode_pair(0b10, 0b10, 0, 1, 2, 0, 1);
        let desc = decode_pair(insn).expect("stp should decode");
        assert!(desc.is_store);
        assert_eq!(desc.size, 8);
        assert_eq!(desc.writeback, Writeback::None);
        assert_eq!(desc.offset, 8);
        assert_eq!(desc.rn, 2);
        assert_eq!(desc.rt, 0);
        assert_eq!(desc.rt2, 1);
        assert_eq!(desc.rt_size, InstructionRegisterSize::Instruction64bit);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn decode_pair_ldp_post_index_32() {
        let insn = encode_pair(0b00, 0b01, 1, 2, 8, 6, 7);
        let desc = decode_pair(insn).expect("ldp (post-index) should decode");
        assert!(!desc.is_store);
        assert_eq!(desc.size, 4);
        assert_eq!(desc.writeback, Writeback::Post);
        assert_eq!(desc.offset, 8);
        assert_eq!(desc.rn, 8);
        assert_eq!(desc.rt, 6);
        assert_eq!(desc.rt2, 7);
        assert_eq!(desc.rt_size, InstructionRegisterSize::Instruction32bit);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn decode_pair_ldp_pre_index_64_negative_offset() {
        let insn = encode_pair(0b10, 0b11, 1, -2, 4, 2, 3);
        let desc = decode_pair(insn).expect("ldp (pre-index) should decode");
        assert!(!desc.is_store);
        assert_eq!(desc.size, 8);
        assert_eq!(desc.writeback, Writeback::Pre);
        assert_eq!(desc.offset, -16);
        assert_eq!(desc.rn, 4);
        assert_eq!(desc.rt, 2);
        assert_eq!(desc.rt2, 3);
        assert_eq!(desc.rt_size, InstructionRegisterSize::Instruction64bit);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn decode_pair_rejects_ldpsw() {
        let insn = encode_pair(0b01, 0b10, 1, 0, 0, 1, 2);
        assert!(decode_pair(insn).is_none());
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn decode_pair_rejects_ldnp_stnp() {
        let insn = encode_pair(0b10, 0b00, 0, 0, 0, 0, 1);
        assert!(decode_pair(insn).is_none());
    }

    const MEM_SIZE: usize = (STAGE2_PAGE_SIZE as usize) * 2;
    static mut TEST_MEM: [u8; MEM_SIZE] = [0; MEM_SIZE];
    static mut READ_COUNT: usize = 0;
    static mut PAIR_READS: usize = 0;
    static mut FAIL_SECOND_READ: bool = false;

    fn test_probe(_ctx: *const (), ipa: u64, size: u8, _is_write: bool) -> bool {
        let end = match ipa.checked_add(size as u64) {
            Some(v) => v,
            None => return false,
        };
        matches!(size, 1 | 2 | 4 | 8) && end <= MEM_SIZE as u64
    }

    fn test_read(_ctx: *const (), ipa: u64, size: u8) -> Result<u64, MmioError> {
        let end = (ipa as usize).saturating_add(size as usize);
        if end > MEM_SIZE {
            return Err(MmioError::Unhandled);
        }
        let mut val = 0u64;
        for i in 0..size {
            let byte = unsafe { TEST_MEM[ipa as usize + i as usize] } as u64;
            val |= byte << (i * 8);
        }
        Ok(val)
    }

    fn test_read_counting(_ctx: *const (), ipa: u64, size: u8) -> Result<u64, MmioError> {
        unsafe { READ_COUNT += 1 };
        test_read(core::ptr::null(), ipa, size)
    }

    fn test_read_pair_fallback(_ctx: *const (), ipa: u64, size: u8) -> Result<u64, MmioError> {
        unsafe { PAIR_READS += 1 };
        if unsafe { FAIL_SECOND_READ } && unsafe { PAIR_READS } == 2 {
            return Err(MmioError::Unhandled);
        }
        test_read(core::ptr::null(), ipa, size)
    }

    fn test_write(_ctx: *const (), ipa: u64, size: u8, value: u64) -> Result<(), MmioError> {
        let end = (ipa as usize).saturating_add(size as usize);
        if end > MEM_SIZE {
            return Err(MmioError::Unhandled);
        }
        for i in 0..size {
            let byte = ((value >> (i * 8)) & 0xff) as u8;
            unsafe {
                TEST_MEM[ipa as usize + i as usize] = byte;
            }
        }
        Ok(())
    }

    struct TestMmioHandler {
        read: fn(*const (), u64, u8) -> Result<u64, MmioError>,
        write: fn(*const (), u64, u8, u64) -> Result<(), MmioError>,
        probe: Option<fn(*const (), u64, u8, bool) -> bool>,
        access_class: AccessClass,
        split_policy: SplitPolicy,
    }

    impl MmioHandler for TestMmioHandler {
        fn read(&self, ipa: u64, size: u8) -> Result<u64, MmioError> {
            (self.read)(core::ptr::null(), ipa, size)
        }

        fn write(&self, ipa: u64, size: u8, value: u64) -> Result<(), MmioError> {
            (self.write)(core::ptr::null(), ipa, size, value)
        }

        fn probe_subaccess(&self, ipa: u64, size: u8, is_write: bool) -> bool {
            self.probe
                .is_some_and(|probe| probe(core::ptr::null(), ipa, size, is_write))
        }

        fn access_class(&self) -> AccessClass {
            self.access_class
        }

        fn split_policy(&self) -> SplitPolicy {
            self.split_policy
        }
    }

    fn make_handler(
        probe: Option<fn(*const (), u64, u8, bool) -> bool>,
        read: fn(*const (), u64, u8) -> Result<u64, MmioError>,
        write: fn(*const (), u64, u8, u64) -> Result<(), MmioError>,
    ) -> TestMmioHandler {
        TestMmioHandler {
            read,
            write,
            probe,
            access_class: AccessClass::DeviceMmio,
            split_policy: SplitPolicy::OnlyIfProbe,
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn split_load_cross_page_unaligned() {
        unsafe {
            TEST_MEM = [0; MEM_SIZE];
        }
        let ipa = STAGE2_PAGE_SIZE - 2;
        unsafe {
            TEST_MEM[ipa as usize] = 0x11;
            TEST_MEM[ipa as usize + 1] = 0x22;
            TEST_MEM[ipa as usize + 2] = 0x33;
            TEST_MEM[ipa as usize + 3] = 0x44;
        }

        let handler = make_handler(Some(test_probe), test_read, test_write);
        let mut regs = [0u64; 32];
        let desc = SingleDesc {
            rn: 0,
            rt: 1,
            rt_size: Instruction64bit,
            size: 4,
            sign_extend: false,
            offset: 0,
            addr_mode: SingleAddrMode::UnsignedOffset,
            is_store: false,
        };
        let split = plan_split_access(ipa, desc.size).expect("split plan");

        let outcome = emulate_single_load(&mut regs, &desc, ipa, Some(&split), &handler);
        assert!(matches!(outcome, EmulationOutcome::Handled));
        assert_eq!(regs[1], 0x44_33_22_11);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn split_store_cross_page_updates_all_bytes_and_writeback() {
        unsafe {
            TEST_MEM = [0; MEM_SIZE];
        }
        let ipa = STAGE2_PAGE_SIZE - 3;
        let value = 0x88_77_66_55_44_33_22_11;
        let handler = make_handler(Some(test_probe), test_read, test_write);
        let mut regs = [0u64; 32];
        regs[2] = value;
        regs[3] = 0x1000;
        let desc = SingleDesc {
            rn: 3,
            rt: 2,
            rt_size: Instruction64bit,
            size: 8,
            sign_extend: false,
            offset: 8,
            addr_mode: SingleAddrMode::PostIndex,
            is_store: true,
        };
        let split = plan_split_access(ipa, desc.size).expect("split plan");

        let outcome = emulate_single_store(&mut regs, &desc, ipa, Some(&split), &handler);
        assert!(matches!(outcome, EmulationOutcome::Handled));
        assert_eq!(regs[3], 0x1000 + 8);

        // First page tail
        assert_eq!(unsafe { TEST_MEM[ipa as usize] }, 0x11);
        assert_eq!(unsafe { TEST_MEM[ipa as usize + 1] }, 0x22);
        assert_eq!(unsafe { TEST_MEM[ipa as usize + 2] }, 0x33);
        // Second page head
        assert_eq!(unsafe { TEST_MEM[ipa as usize + 3] }, 0x44);
        assert_eq!(unsafe { TEST_MEM[ipa as usize + 4] }, 0x55);
        assert_eq!(unsafe { TEST_MEM[ipa as usize + 5] }, 0x66);
        assert_eq!(unsafe { TEST_MEM[ipa as usize + 6] }, 0x77);
        assert_eq!(unsafe { TEST_MEM[ipa as usize + 7] }, 0x88);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn pair_load_fallback_reads_both_before_writing() {
        unsafe {
            TEST_MEM = [0; MEM_SIZE];
            TEST_MEM[0x40] = 0xaa;
            TEST_MEM[0x48] = 0xbb;
            PAIR_READS = 0;
            FAIL_SECOND_READ = false;
        }
        let handler = make_handler(Some(test_probe), test_read, test_write);
        let mut regs = [0u64; 32];
        regs[0] = 0;
        regs[1] = 0;
        let desc = PairDesc {
            rn: 0,
            rt: 0,
            rt2: 1,
            rt_size: Instruction64bit,
            size: 8,
            offset: 0,
            writeback: Writeback::None,
            is_store: false,
        };
        let outcome = emulate_pair_load(&mut regs, &desc, 0x40, 0x48, None, None, &handler);
        assert!(matches!(outcome, EmulationOutcome::Handled));
        assert_eq!(regs[0], 0xaa);
        assert_eq!(regs[1], 0xbb);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn pair_load_fallback_aborts_on_second_read_failure() {
        unsafe {
            TEST_MEM = [0; MEM_SIZE];
            TEST_MEM[0x40] = 0xaa;
            TEST_MEM[0x48] = 0xbb;
            PAIR_READS = 0;
            FAIL_SECOND_READ = true;
        }
        let handler = make_handler(Some(test_probe), test_read_pair_fallback, test_write);
        let mut regs = [0u64; 32];
        let desc = PairDesc {
            rn: 0,
            rt: 0,
            rt2: 1,
            rt_size: Instruction64bit,
            size: 8,
            offset: 0,
            writeback: Writeback::None,
            is_store: false,
        };
        let outcome = emulate_pair_load(&mut regs, &desc, 0x40, 0x48, None, None, &handler);
        assert!(matches!(outcome, EmulationOutcome::NotHandled));
        assert_eq!(unsafe { PAIR_READS }, 2);
        assert_eq!(regs[0], 0);
        assert_eq!(regs[1], 0);
        unsafe { FAIL_SECOND_READ = false };
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn split_path_rejects_without_probe_and_leaves_mem_untouched() {
        unsafe {
            TEST_MEM = [0; MEM_SIZE];
            READ_COUNT = 0;
        }
        let ipa = STAGE2_PAGE_SIZE - 1;
        let handler = make_handler(None, test_read_counting, test_write);
        let mut regs = [0u64; 32];
        let desc = SingleDesc {
            rn: 0,
            rt: 1,
            rt_size: Instruction32bit,
            size: 4,
            sign_extend: false,
            offset: 0,
            addr_mode: SingleAddrMode::UnsignedOffset,
            is_store: false,
        };

        let outcome = emulate_single_load(&mut regs, &desc, ipa, None, &handler);
        assert!(matches!(outcome, EmulationOutcome::NotHandled));
        assert_eq!(unsafe { READ_COUNT }, 0);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn execute_mmio_advances_elr_on_success() {
        static mut LAST_WRITE: (u64, u8, u64) = (0, 0, 0);
        fn read_stub(_: *const (), _: u64, _: u8) -> Result<u64, MmioError> {
            Ok(0)
        }
        fn write_record(_: *const (), ipa: u64, size: u8, value: u64) -> Result<(), MmioError> {
            unsafe { LAST_WRITE = (ipa, size, value) };
            Ok(())
        }
        let handler = TestMmioHandler {
            read: read_stub,
            write: write_record,
            probe: None,
            access_class: AccessClass::NormalMemory,
            split_policy: SplitPolicy::Never,
        };
        let mut regs = zero_regs();
        regs.gprs_mut()[2] = 0xaaaa_5555;
        let desc = SingleDesc {
            rn: 0,
            rt: 2,
            rt_size: Instruction64bit,
            size: 8,
            sign_extend: false,
            offset: 0,
            addr_mode: SingleAddrMode::UnsignedOffset,
            is_store: true,
        };
        let decoded = MmioDecoded::Single {
            desc,
            fault_va: 0,
            ipa: 0x4000,
            split: None,
        };
        let info = info_with_esr(ESR_EL2::from_bits(0), 0, Some(0x4000));
        write_elr_el2(0x800);
        let outcome = execute_mmio(&mut regs, &info, &handler, &decoded);
        assert!(matches!(outcome, EmulationOutcome::Handled));
        assert_eq!(read_elr_el2(), 0x804);
        unsafe {
            let snapshot = LAST_WRITE;
            assert_eq!(snapshot, (0x4000, 8, 0xaaaa_5555));
            LAST_WRITE = (0, 0, 0);
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn execute_mmio_keeps_elr_on_failure() {
        fn read_stub(_: *const (), _: u64, _: u8) -> Result<u64, MmioError> {
            Ok(0)
        }
        fn write_fail(_: *const (), _: u64, _: u8, _: u64) -> Result<(), MmioError> {
            Err(MmioError::Unhandled)
        }
        let handler = TestMmioHandler {
            read: read_stub,
            write: write_fail,
            probe: None,
            access_class: AccessClass::NormalMemory,
            split_policy: SplitPolicy::Never,
        };
        let mut regs = zero_regs();
        regs.gprs_mut()[2] = 0x1234;
        let desc = SingleDesc {
            rn: 0,
            rt: 2,
            rt_size: Instruction64bit,
            size: 8,
            sign_extend: false,
            offset: 0,
            addr_mode: SingleAddrMode::UnsignedOffset,
            is_store: true,
        };
        let decoded = MmioDecoded::Single {
            desc,
            fault_va: 0,
            ipa: 0x4000,
            split: None,
        };
        let info = info_with_esr(ESR_EL2::from_bits(0), 0, Some(0x4000));
        write_elr_el2(0x900);
        let outcome = execute_mmio(&mut regs, &info, &handler, &decoded);
        assert!(matches!(outcome, EmulationOutcome::NotHandled));
        assert_eq!(read_elr_el2(), 0x900);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn try_emulate_mmio_leaves_elr_when_not_handled() {
        let esr = {
            let tmp = ESR_EL2::from_bits(DataFaultStatusCade::TranslationLevel0 as u64);
            tmp.set(ESR_EL2::fnv, 1);
            tmp
        };
        let info = info_with_esr(esr, 0x1000, Some(0x2000));
        let mut regs = zero_regs();
        fn read_stub(_: *const (), _: u64, _: u8) -> Result<u64, MmioError> {
            Ok(0)
        }
        fn write_stub(_: *const (), _: u64, _: u8, _: u64) -> Result<(), MmioError> {
            Ok(())
        }
        let handler = TestMmioHandler {
            read: read_stub,
            write: write_stub,
            probe: None,
            access_class: AccessClass::NormalMemory,
            split_policy: SplitPolicy::Never,
        };
        write_elr_el2(0x100);
        let outcome = try_emulate_mmio(&mut regs, &info, &handler);
        assert!(matches!(outcome, EmulationOutcome::NotHandled));
        assert_eq!(read_elr_el2(), 0x100);
    }
}
