use crate::emulation::EmulationOutcome;
use crate::emulation::PairDesc;
use crate::emulation::SingleDesc;
use crate::emulation::SplitPlan;
use crate::emulation::apply_pair_writeback;
use crate::emulation::apply_writeback;
use crate::emulation::can_handle_plan;
use crate::emulation::crosses_stage2_page;
use crate::memory_hook::MmioError;
use crate::memory_hook::MmioHandler;

pub(crate) fn emulate_single(
    regs: &mut [u64; 32],
    desc: &SingleDesc,
    ipa: u64,
    split: Option<&SplitPlan>,
    handler: &dyn MmioHandler,
) -> EmulationOutcome {
    let value = read_reg(regs, desc.rt, desc.size);

    if !write_value(handler, ipa, desc.size, split, value) {
        return EmulationOutcome::NotHandled;
    }
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
    let v0 = read_reg(regs, desc.rt, desc.size);
    let v1 = read_reg(regs, desc.rt2, desc.size);

    let use_pair_ops = split0.is_none()
        && split1.is_none()
        && (desc.size <= 1 || (ipa0 % desc.size as u64 == 0 && ipa1 % desc.size as u64 == 0));

    if use_pair_ops {
        if let Some(result) = handler.write_pair(ipa0, ipa1, desc.size, v0, v1) {
            // Execute as a single MMIO transaction when possible to avoid partial side effects.
            // If the hook is missing or fails, we do not attempt rollback and instead mark
            // the access as NotHandled so the upper layer can decide how to proceed.
            match result {
                Ok(()) => {}
                Err(MmioError::Unhandled) | Err(MmioError::Fault) => {
                    return EmulationOutcome::NotHandled;
                }
            }
        } else {
            // Best-effort fallback when pair writes are not supported: ensure both sub-writes
            // are accepted before issuing either one. This is not atomic across the two
            // addresses, so callers relying on pair atomicity should provide write_pair.
            if !handler.probe_subaccess(ipa0, desc.size, true)
                || !handler.probe_subaccess(ipa1, desc.size, true)
            {
                return EmulationOutcome::NotHandled;
            }
            match handler.write(ipa0, desc.size, v0) {
                Ok(_) => {}
                Err(_) => return EmulationOutcome::NotHandled,
            }
            match handler.write(ipa1, desc.size, v1) {
                Ok(_) => {}
                Err(_) => return EmulationOutcome::NotHandled,
            }
        }
    } else {
        if !write_value(handler, ipa0, desc.size, split0, v0)
            || !write_value(handler, ipa1, desc.size, split1, v1)
        {
            return EmulationOutcome::NotHandled;
        }
    }

    apply_pair_writeback(regs, desc.rn, desc.offset, desc.writeback);
    EmulationOutcome::Handled
}

fn read_reg(regs: &[u64; 32], reg: u8, size: u8) -> u64 {
    let val = if reg == 31 { 0 } else { regs[reg as usize] };
    match size {
        1 => val & 0xff,
        2 => val & 0xffff,
        4 => val & 0xffff_ffff,
        _ => val,
    }
}

fn execute_split_store(handler: &dyn MmioHandler, plan: &SplitPlan, value_total: u64) -> bool {
    for seg in plan.segments() {
        let seg_mask = mask_for_size(seg.size);
        let seg_val = (value_total >> (seg.byte_offset as u64 * 8)) & seg_mask;
        match handler.write(seg.ipa, seg.size, seg_val) {
            Ok(_) => {}
            Err(MmioError::Unhandled) | Err(MmioError::Fault) => return false,
        }
    }
    true
}

fn mask_for_size(size: u8) -> u64 {
    match size {
        1 => 0xff,
        2 => 0xffff,
        4 => 0xffff_ffff,
        8 => u64::MAX,
        _ => 0,
    }
}

fn write_value(
    handler: &dyn MmioHandler,
    ipa: u64,
    size: u8,
    split: Option<&SplitPlan>,
    value_total: u64,
) -> bool {
    if let Some(plan) = split {
        if !can_handle_plan(handler, plan, true) {
            return false;
        }
        return execute_split_store(handler, plan, value_total);
    }

    let needs_split = size > 1 && (ipa % size as u64 != 0 || crosses_stage2_page(ipa, size));
    if needs_split {
        // Split plans must be supplied by decode to enforce guest VA boundary checks.
        return false;
    }

    match handler.write(ipa, size, value_total) {
        Ok(_) => true,
        Err(MmioError::Unhandled) | Err(MmioError::Fault) => false,
    }
}
