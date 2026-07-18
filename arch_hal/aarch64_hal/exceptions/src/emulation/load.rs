use crate::emulation::EmulationOutcome;
use crate::emulation::PairDesc;
use crate::emulation::SingleAddrMode;
use crate::emulation::SingleDesc;
use crate::emulation::SplitPlan;
use crate::emulation::base_writeback;
use crate::emulation::can_handle_plan;
use crate::emulation::crosses_stage2_page;
use crate::memory_hook::MmioError;
use crate::memory_hook::MmioHandler;
use crate::registers::InstructionRegisterSize;
use cpu::set_sp_el1;

pub(crate) fn emulate_single(
    regs: &mut [u64; 32],
    desc: &SingleDesc,
    ipa: u64,
    split: Option<&SplitPlan>,
    handler: &dyn MmioHandler,
) -> EmulationOutcome {
    let raw_total = match read_value(handler, ipa, desc.size, split) {
        Some(v) => v,
        None => return EmulationOutcome::NotHandled,
    };

    let val = extend_loaded_value(raw_total, desc.size, desc.sign_extend, desc.rt_size);

    write_reg(regs, desc.rt, val, desc.rt_size);
    apply_writeback(regs, desc.rn, desc.offset, desc.addr_mode);
    EmulationOutcome::Handled
}

pub(crate) fn emulate_pair(
    regs: &mut [u64; 32],
    desc: &PairDesc,
    ipa0: u64,
    ipa1: u64,
    split0: Option<&SplitPlan>,
    split1: Option<&SplitPlan>,
    handler: &dyn MmioHandler,
) -> EmulationOutcome {
    let use_pair_ops = split0.is_none()
        && split1.is_none()
        && (desc.size <= 1 || (ipa0 % desc.size as u64 == 0 && ipa1 % desc.size as u64 == 0));

    let (v0, v1) = if use_pair_ops {
        if let Some(result) = handler.read_pair(ipa0, ipa1, desc.size) {
            match result {
                Ok(v) => v,
                Err(MmioError::Unhandled) | Err(MmioError::Fault) => {
                    return EmulationOutcome::NotHandled;
                }
            }
        } else {
            if !handler.probe_subaccess(ipa0, desc.size, false)
                || !handler.probe_subaccess(ipa1, desc.size, false)
            {
                return EmulationOutcome::NotHandled;
            }
            let first = match handler.read(ipa0, desc.size) {
                Ok(v) => v,
                Err(_) => return EmulationOutcome::NotHandled,
            };
            let second = match handler.read(ipa1, desc.size) {
                Ok(v) => v,
                Err(_) => return EmulationOutcome::NotHandled,
            };
            (first, second)
        }
    } else {
        let first = match read_value(handler, ipa0, desc.size, split0) {
            Some(v) => v,
            None => return EmulationOutcome::NotHandled,
        };
        let second = match read_value(handler, ipa1, desc.size, split1) {
            Some(v) => v,
            None => return EmulationOutcome::NotHandled,
        };
        (first, second)
    };

    write_reg(
        regs,
        desc.rt,
        extend_loaded_value(v0, desc.size, false, desc.rt_size),
        desc.rt_size,
    );
    write_reg(
        regs,
        desc.rt2,
        extend_loaded_value(v1, desc.size, false, desc.rt_size),
        desc.rt_size,
    );
    apply_pair_writeback(regs, desc.rn, desc.offset, desc.writeback);
    EmulationOutcome::Handled
}

fn extend_loaded_value(
    raw: u64,
    size: u8,
    sign_extend: bool,
    rt_size: InstructionRegisterSize,
) -> u64 {
    let masked = mask_value(raw, size);
    match (sign_extend, rt_size) {
        (true, InstructionRegisterSize::Instruction64bit) => sign_extend_value(masked, size),
        (true, InstructionRegisterSize::Instruction32bit) => {
            let v32 = sign_extend_to_32(masked, size);
            v32 as u64
        }
        (false, InstructionRegisterSize::Instruction64bit) => masked,
        (false, InstructionRegisterSize::Instruction32bit) => (masked as u32) as u64,
    }
}

/// Test-only helper for exercising load extension logic from integration tests.
#[cfg(feature = "testapi")]
pub fn test_extend_loaded_value(
    raw: u64,
    size: u8,
    sign_extend: bool,
    rt_size: InstructionRegisterSize,
) -> u64 {
    extend_loaded_value(raw, size, sign_extend, rt_size)
}

fn assemble_split_value(handler: &dyn MmioHandler, plan: &SplitPlan) -> Option<u64> {
    let mut total = 0u64;
    for seg in plan.segments() {
        let raw = match handler.read(seg.ipa, seg.size) {
            Ok(v) => v,
            Err(MmioError::Unhandled) | Err(MmioError::Fault) => return None,
        };
        let masked = mask_value(raw, seg.size);
        total |= masked << (seg.byte_offset as u64 * 8);
    }
    Some(total)
}

fn execute_split_load(handler: &dyn MmioHandler, plan: &SplitPlan, total_size: u8) -> Option<u64> {
    let assembled = assemble_split_value(handler, plan)?;
    Some(mask_value(assembled, total_size))
}

fn read_value(
    handler: &dyn MmioHandler,
    ipa: u64,
    size: u8,
    split: Option<&SplitPlan>,
) -> Option<u64> {
    if let Some(plan) = split {
        if !can_handle_plan(handler, plan, false) {
            return None;
        }
        return execute_split_load(handler, plan, size);
    }

    let needs_split = size > 1 && (ipa % size as u64 != 0 || crosses_stage2_page(ipa, size));
    if needs_split {
        // Split plans must be supplied by decode to enforce guest VA boundary checks.
        return None;
    }

    match handler.read(ipa, size) {
        Ok(v) => Some(v),
        Err(MmioError::Unhandled) | Err(MmioError::Fault) => None,
    }
}

fn mask_value(val: u64, size: u8) -> u64 {
    match size {
        1 => val & 0xff,
        2 => val & 0xffff,
        4 => val & 0xffff_ffff,
        _ => val,
    }
}

fn sign_extend_to_32(val: u64, size: u8) -> u32 {
    match size {
        1 => (val as i8 as i32) as u32,
        2 => (val as i16 as i32) as u32,
        4 => (val as i32) as u32,
        8 => (val as i64 as i32) as u32,
        _ => val as u32,
    }
}

fn sign_extend_value(val: u64, size: u8) -> u64 {
    match size {
        1 => (val as i8 as i64) as u64,
        2 => (val as i16 as i64) as u64,
        4 => (val as i32 as i64) as u64,
        8 => (val as i64) as u64,
        _ => val,
    }
}

fn write_reg(regs: &mut [u64; 32], reg: u8, value: u64, reg_size: InstructionRegisterSize) {
    if reg >= 32 || reg == 31 {
        return;
    }
    let val = match reg_size {
        InstructionRegisterSize::Instruction32bit => value & 0xffff_ffff,
        InstructionRegisterSize::Instruction64bit => value,
    };
    regs[reg as usize] = val;
}

fn apply_writeback(regs: &mut [u64; 32], rn: u8, offset: i64, mode: SingleAddrMode) {
    if matches!(mode, SingleAddrMode::PostIndex | SingleAddrMode::PreIndex) {
        let base = base_writeback(regs, rn);
        let new_base = base.wrapping_add(offset as u64);
        write_back_base(regs, rn, new_base);
    }
}

fn apply_pair_writeback(
    regs: &mut [u64; 32],
    rn: u8,
    offset: i64,
    mode: crate::emulation::Writeback,
) {
    if matches!(
        mode,
        crate::emulation::Writeback::Post | crate::emulation::Writeback::Pre
    ) {
        let base = base_writeback(regs, rn);
        let new_base = base.wrapping_add(offset as u64);
        write_back_base(regs, rn, new_base);
    }
}

fn write_back_base(regs: &mut [u64; 32], rn: u8, val: u64) {
    if rn == 31 {
        set_sp_el1(val);
    } else {
        regs[rn as usize] = val;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registers::InstructionRegisterSize::Instruction32bit;
    use crate::registers::InstructionRegisterSize::Instruction64bit;

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn sign_extends_byte_into_w_zeroes_upper() {
        let v = extend_loaded_value(0xff, 1, true, Instruction32bit);
        assert_eq!(v, 0x0000_0000_ffff_ffff);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn sign_extends_halfword_into_x() {
        let v = extend_loaded_value(0x8000, 2, true, Instruction64bit);
        assert_eq!(v, 0xffff_ffff_ffff_8000);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(all(test, not(target_arch = "aarch64")), test)]
    fn sign_extends_word_into_x() {
        let v = extend_loaded_value(0x8000_0001, 4, true, Instruction64bit);
        assert_eq!(v, 0xffff_ffff_8000_0001);
    }
}
