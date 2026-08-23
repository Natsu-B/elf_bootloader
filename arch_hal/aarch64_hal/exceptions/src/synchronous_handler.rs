use core::cell::SyncUnsafeCell;

use crate::emulation;
use crate::emulation::hpfar_el2_written_for_abort;
use crate::emulation::is_prfm;
use crate::registers::ESR_EL2;
use crate::registers::ExceptionClass;
use crate::registers::InstructionRegisterSize;
use crate::registers::SyndromeAccessSize;
use crate::registers::SysRegIss;
use crate::registers::TI;
use crate::registers::WriteNotRead;
use cpu::Registers;
use cpu::fault_ipa_el2;
use cpu::get_elr_el2;
use cpu::get_far_el2;
use cpu::read_guest_insn_u32_at_el1_pc;
use print::println;
use psci::handle_secure_monitor_call;

pub type InstructionAbortHandler = fn(regs: &mut Registers, info: &InstructionAbortInfo);

#[derive(Copy, Clone, Debug)]
pub struct InstructionAbortInfo {
    pub esr_el2: ESR_EL2,
    pub elr_el2: u64,
    pub far_el2: u64,
    pub fault_ipa: Option<u64>,
    pub ifsc: Option<u64>,
}

/// Handles a decoded data abort.
///
/// Registered handlers must support concurrent invocation on multiple cores.
pub type DataAbortHandler = fn(&mut Registers, &DataAbortInfo, Option<&emulation::MmioDecoded>);
pub type DebugExceptionHandler = fn(&mut Registers, ExceptionClass);
pub type SysRegTrapHandler = fn(&mut Registers, &SysRegTrapInfo);
pub type TrappedWfHandler = fn(&mut Registers, &TrappedWfInfo);

#[derive(Copy, Clone, Debug)]
pub enum DataAbortAccessSource {
    Iss,
    Instruction,
}

#[derive(Copy, Clone, Debug)]
pub struct DataAbortAccess {
    pub reg_num: usize,
    pub reg_size: InstructionRegisterSize,
    pub access_width: SyndromeAccessSize,
    pub write_access: WriteNotRead,
    pub source: DataAbortAccessSource,
}

#[derive(Copy, Clone, Debug)]
pub struct DataAbortInfo {
    pub esr_el2: ESR_EL2,
    pub far_el2: u64,
    pub fault_ipa: Option<u64>,
    pub access: Option<DataAbortAccess>,
}

#[derive(Copy, Clone, Debug)]
pub struct SysRegTrapInfo {
    pub esr_el2: ESR_EL2,
    pub iss: SysRegIss,
}

#[derive(Copy, Clone, Debug)]
pub struct TrappedWfInfo {
    pub esr_el2: ESR_EL2,
    pub elr_el2: u64,
    pub ti: TI,
}

impl DataAbortInfo {
    pub fn register_mut<'a>(&self, regs: &'a mut Registers) -> Option<&'a mut u64> {
        let access = self.access?;
        if access.reg_num >= 32 {
            return None;
        }
        regs.gpr_mut(access.reg_num as u8)
    }
}

static SYNCHRONOUS_HANDLER: SyncUnsafeCell<SynchronousHandler> =
    SyncUnsafeCell::new(SynchronousHandler {
        data_abort_func: None,
        debug_func: None,
        sysreg_trap_func: None,
        instruction_abort_func: None,
        trapped_wf_func: None,
    });

struct SynchronousHandler {
    data_abort_func: Option<DataAbortHandler>,
    debug_func: Option<DebugExceptionHandler>,
    sysreg_trap_func: Option<SysRegTrapHandler>,
    instruction_abort_func: Option<InstructionAbortHandler>,
    trapped_wf_func: Option<TrappedWfHandler>,
}

pub fn set_data_abort_handler(handler: DataAbortHandler) {
    unsafe { &mut *SYNCHRONOUS_HANDLER.get() }.data_abort_func = Some(handler);
}

pub fn set_debug_handler(handler: DebugExceptionHandler) {
    unsafe { &mut *SYNCHRONOUS_HANDLER.get() }.debug_func = Some(handler);
}

pub fn set_sysreg_trap_handler(handler: SysRegTrapHandler) {
    unsafe { &mut *SYNCHRONOUS_HANDLER.get() }.sysreg_trap_func = Some(handler);
}

pub fn set_instruction_abort_handler(handler: InstructionAbortHandler) {
    unsafe { &mut *SYNCHRONOUS_HANDLER.get() }.instruction_abort_func = Some(handler);
}

pub fn set_trapped_wf_handler(handler: TrappedWfHandler) {
    unsafe { &mut *SYNCHRONOUS_HANDLER.get() }.trapped_wf_func = Some(handler);
}

pub(crate) extern "C" fn synchronous_handler(reg: *mut Registers) {
    let reg = unsafe { &mut *reg };
    let esr_el2 = ESR_EL2::from_bits(cpu::get_esr_el2());
    match esr_el2.get_enum::<_, ExceptionClass>(ESR_EL2::ec) {
        Some(ec) => {
            match ec {
                ExceptionClass::DataAbortFormLowerLevel => {
                    let access = if esr_el2.get(ESR_EL2::isv) != 0 {
                        decode_access_from_esr(&esr_el2)
                    } else {
                        decode_access_from_insn()
                    };
                    let info = DataAbortInfo {
                        esr_el2,
                        far_el2: get_far_el2(),
                        fault_ipa: fault_ipa_hint(&esr_el2),
                        access,
                    };
                    let decoded = emulation::decode_mmio(&*reg, &info);

                    match unsafe { &*SYNCHRONOUS_HANDLER.get() }.data_abort_func {
                        Some(handler) => handler(reg, &info, decoded.as_ref()),
                        None => panic!("Data Abort handler is not registered"),
                    }
                }
                ExceptionClass::SMCInstructionExecution => {
                    let imm16 = esr_el2.get(ESR_EL2::imm16);
                    if imm16 != 0 {
                        // vender specific smc call
                        println!("unknown SMC imm value: 0x{:X}", imm16);
                        reg.x0 = u64::MAX; // SMCCC_RET_NOT_SUPPORTED(-1)
                        cpu::set_elr_el2(cpu::get_elr_el2().wrapping_add(4));
                        return;
                    }
                    handle_secure_monitor_call(reg);
                }
                ExceptionClass::InstructionAbortFromLowerLevel => {
                    let handler = unsafe { &mut *SYNCHRONOUS_HANDLER.get() };
                    let elr_el2 = get_elr_el2();
                    let far_el2 = get_far_el2();
                    let fault_ipa = fault_ipa_hint(&esr_el2);
                    let ifsc = Some(esr_el2.get(ESR_EL2::dfsc));
                    let info = InstructionAbortInfo {
                        esr_el2,
                        elr_el2,
                        far_el2,
                        fault_ipa,
                        ifsc,
                    };
                    if let Some(f) = handler.instruction_abort_func {
                        f(reg, &info);
                    } else {
                        println!(
                            "EL2: guest instruction abort (no handler) elr=0x{:X} far=0x{:X} ipa_hint={:?} esr=0x{:X}",
                            elr_el2,
                            far_el2,
                            fault_ipa,
                            esr_el2.bits()
                        );
                    }
                }
                ExceptionClass::TrappedWFInstruction => {
                    let handler = unsafe { &*SYNCHRONOUS_HANDLER.get() };
                    let ti = esr_el2.get_enum(ESR_EL2::ti).unwrap_or(TI::WFI);
                    let info = TrappedWfInfo {
                        esr_el2,
                        elr_el2: get_elr_el2(),
                        ti,
                    };
                    if let Some(f) = handler.trapped_wf_func {
                        f(reg, &info);
                    } else {
                        cpu::set_elr_el2(cpu::get_elr_el2().wrapping_add(4));
                    }
                }
                ExceptionClass::BreakpointLowerLevel
                | ExceptionClass::BrkInstructionAArch64LowerLevel
                | ExceptionClass::SoftwareStepLowerLevel
                | ExceptionClass::WatchpointLowerLevel => {
                    unsafe { &*SYNCHRONOUS_HANDLER.get() }
                        .debug_func
                        .expect("debug handler not registered")(reg, ec);
                }
                ExceptionClass::SystemRegisterTrap => {
                    let info = SysRegTrapInfo {
                        esr_el2,
                        iss: SysRegIss::from_esr(&esr_el2),
                    };
                    unsafe { &*SYNCHRONOUS_HANDLER.get() }
                        .sysreg_trap_func
                        .expect("sysreg trap handler not registered")(
                        reg, &info
                    );
                }
                _ => panic!("unexpected ESR_EL2 EC value: {:?}", ec),
            }
        }
        _ => panic!("unkown ESR_EL2 EC value: {}", esr_el2.get(ESR_EL2::ec)),
    }
}

fn fault_ipa_hint(esr_el2: &ESR_EL2) -> Option<u64> {
    if esr_el2.get(ESR_EL2::fnv) != 0 {
        return None;
    }
    hpfar_el2_written_for_abort(esr_el2).then(|| fault_ipa_el2())
}

fn decode_access_from_esr(esr_el2: &ESR_EL2) -> Option<DataAbortAccess> {
    Some(DataAbortAccess {
        reg_num: esr_el2.get(ESR_EL2::srt) as usize,
        reg_size: esr_el2.get_enum(ESR_EL2::sf)?,
        access_width: esr_el2.get_enum(ESR_EL2::sas)?,
        write_access: esr_el2.get_enum(ESR_EL2::wnr)?,
        source: DataAbortAccessSource::Iss,
    })
}

fn decode_access_from_insn() -> Option<DataAbortAccess> {
    let insn = read_guest_insn_u32_at_el1_pc(get_elr_el2())?;
    decode_load_store(insn)
}

fn decode_load_store(insn: u32) -> Option<DataAbortAccess> {
    if is_prfm(insn) {
        return None;
    }
    const LS_MASK: u32 = 0x3b00_0000;
    match insn & LS_MASK {
        0x3800_0000 | 0x3900_0000 => {}
        _ => return None,
    }

    let opc = (insn >> 22) & 0x3;

    let size = (insn >> 30) & 0x3;
    let access_width = match size {
        0 => SyndromeAccessSize::Byte,
        1 => SyndromeAccessSize::HalfWord,
        2 => SyndromeAccessSize::Word,
        3 => SyndromeAccessSize::DoubleWord,
        _ => return None,
    };

    let reg_size = if (opc & 0b10) == 0 {
        if size == 3 {
            InstructionRegisterSize::Instruction64bit
        } else {
            InstructionRegisterSize::Instruction32bit
        }
    } else {
        if size == 3 {
            return None;
        }
        if size == 2 && (opc & 0b1) != 0 {
            return None;
        }
        if (opc & 0b1) != 0 {
            InstructionRegisterSize::Instruction32bit
        } else {
            InstructionRegisterSize::Instruction64bit
        }
    };

    let write_access = if (opc & 0b10) == 0 && opc == 0 {
        WriteNotRead::WritingMemoryAbort
    } else {
        WriteNotRead::ReadingMemoryAbort
    };

    Some(DataAbortAccess {
        reg_num: (insn & 0x1f) as usize,
        reg_size,
        access_width,
        write_access,
        source: DataAbortAccessSource::Instruction,
    })
}
