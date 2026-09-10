//! Startup-IPI delivery interlock for the explicit SMP carrier.
//!
//! VMX root discards SIPIs. A sender must not assume that an INIT-exited target
//! has already reentered WAIT. Only INIT/SIPI need a software completion record;
//! the hardware APIC remains authoritative for ordinary interrupts/registers.

use super::CpuBoot;
use super::Error;
use super::GuestRegisters;
use crate::vmx_smoke as monitor;
use core::sync::atomic::Ordering;
use x86_64_hal::cpu;
use x86_64_hal::vmcs;
use x86_64_hal::vmx;

const MTF: u64 = 1 << 27;

/// Checked private EPT slot. Only its owner modifies the write permission, and
/// no reference or lock survives guest entry. This never changes L1's EPTP.
#[derive(Clone, Copy)]
pub(in crate::vmx_smoke) struct IpiTrap {
    page: u64,
    slot: u64,
    entry: u64,
    eptp: u64,
    stepping: bool,
}

impl IpiTrap {
    pub(in crate::vmx_smoke) fn new(page: u64, slot: u64, entry: u64, eptp: u64) -> Option<Self> {
        (page & 4095 == 0 && slot & 7 == 0 && entry == page | 7).then_some(Self {
            page,
            slot,
            entry,
            eptp,
            stepping: false,
        })
    }

    fn writable(&self, write: bool) -> Result<(), Error> {
        // SAFETY: construction found this aligned 4K UC leaf in the owner's
        // complete private EPT arena. No guest/other CPU writes that arena.
        // Only W changes; address/type/other permissions retain their values.
        unsafe {
            core::ptr::write_volatile(
                self.slot as *mut u64,
                self.entry & !2 | if write { 2 } else { 0 },
            );
            monitor::require(
                "APIC carrier INVEPT",
                vmx::invept(
                    1,
                    &vmx::InveptDescriptor {
                        ept_pointer: self.eptp,
                        reserved: 0,
                    },
                ),
            )
        }
    }
}

pub(super) fn prepare(
    count: usize,
    layout: monitor::MonitorLayout,
    bsp: &monitor::PreparedMonitor,
) -> Result<(), Error> {
    if count == 1 {
        return Ok(());
    }
    // SAFETY: CPL0 firmware preparation established VMX. These immutable
    // capabilities are sampled before any carrier or AP takes ownership.
    let (primary, ept) = unsafe {
        let basic = bsp.monitor.as_ref().runtime.lock().basic;
        (
            cpu::rdmsr(if basic.true_controls {
                vmx::IA32_VMX_TRUE_PROCBASED_CTLS
            } else {
                vmx::IA32_VMX_PROCBASED_CTLS
            }),
            cpu::rdmsr(vmx::IA32_VMX_EPT_VPID_CAP),
        )
    };
    if (primary >> 32) & MTF == 0 || primary & MTF != 0 || ept & (1 << 25) == 0 || bsp.ipi.is_none()
    {
        return Err(Error::Capability("SMP IPI MTF/INVEPT", primary));
    }
    for slot in 0..count {
        let block = layout
            .block(slot)
            .ok_or(Error::Capability("IPI CPU block", slot as u64))?;
        for index in [0x1b_u32, 0x830] {
            let byte = block + monitor::MSR_BITMAP_PAGE * 4096 + 2048 + u64::from(index / 8);
            // SAFETY: each disjoint initialized CPU block contains this whole
            // bitmap; no guest/CPU uses it yet. Only these write intercepts change.
            unsafe {
                let pointer = byte as *mut u8;
                pointer.write_volatile(pointer.read_volatile() | 1 << (index % 8));
            }
        }
    }
    Ok(())
}

pub(super) fn activate(boot: &CpuBoot) -> Result<(), Error> {
    if let Some(trap) = *boot.ipi.lock() {
        trap.writable(false)?;
    }
    Ok(())
}

fn read(field: u32) -> Result<u64, Error> {
    // SAFETY: the owning CPU is in VMX root with its stopped carrier current.
    unsafe { monitor::vmcs_read(field) }.map_err(|s| Error::Instruction("IPI VMREAD", s, 0))
}

fn mtf(enabled: bool) -> Result<(), Error> {
    let controls = read(vmcs::CPU_BASED_VM_EXEC_CONTROL)?;
    monitor::write_vmcs(
        vmcs::CPU_BASED_VM_EXEC_CONTROL,
        controls & !MTF | if enabled { MTF } else { 0 },
    )
}

/// A non-MTF exit cancels the temporary mapping before normal dispatch. An
/// event delivered before the store can therefore never masquerade as a store.
pub(super) fn handle_exit(registers: &mut GuestRegisters, reason: u64) -> bool {
    let boot = &monitor::current_cpu().boot;
    let Some(mut trap) = *boot.ipi.lock() else {
        return false;
    };
    let result = (|| {
        if trap.stepping {
            monitor::write_vmcs(vmcs::VM_ENTRY_INTR_INFO_FIELD, 0)?;
            trap.writable(false)?;
            mtf(false)?;
            trap.stepping = false;
            *boot.ipi.lock() = Some(trap);
            if reason == 37 {
                return Ok(true);
            }
        }
        if reason == 32 && matches!(registers.rcx as u32, 0x1b | 0x830) {
            monitor::write_vmcs(vmcs::VM_ENTRY_INTR_INFO_FIELD, 0)?;
            let value = u64::from(registers.rax as u32) | u64::from(registers.rdx as u32) << 32;
            let index = registers.rcx as u32;
            // SAFETY: CPL0 with CPUID.APIC established, reading real mode/base.
            let apic = unsafe { cpu::rdmsr(0x1b) };
            if index == 0x1b && value & 0x000f_ffff_ffff_f000 != trap.page {
                return Err(Error::Capability("APIC base relocation not mapped", value));
            }
            if index == 0x830 && apic & 0xc00 == 0xc00 {
                monitor::record_diagnostic(monitor::DiagnosticEvent::IpiDecoded { x2: true });
                let sent = deliver(boot, (value >> 32) as u32, value as u32, || {
                    // SAFETY: real enabled x2APIC ICR, exact guest payload. The
                    // private #GP guard recovers an unsupported hardware write.
                    unsafe { x86_64_hal::host_state::try_wrmsr(index, value) }
                })?;
                if !sent {
                    let rip = read(vmcs::GUEST_RIP)?;
                    monitor::inject_general_protection(reason, 0, rip, 0, registers);
                    return Ok(true);
                }
            } else {
                // SAFETY: only APIC_BASE or ICR reaches this branch. The guarded
                // instruction preserves architectural #GP without a root fault.
                if !unsafe { x86_64_hal::host_state::try_wrmsr(index, value) } {
                    let rip = read(vmcs::GUEST_RIP)?;
                    monitor::inject_general_protection(reason, 0, rip, 0, registers);
                    return Ok(true);
                }
            }
            let rip = read(vmcs::GUEST_RIP)?;
            monitor::advance_guest_rip(
                reason,
                0,
                rip,
                read(vmcs::VM_EXIT_INSTRUCTION_LEN)?,
                registers,
            );
            return Ok(true);
        }
        if reason != 48
            || read(vmcs::GUEST_PHYSICAL_ADDRESS)? & !4095 != trap.page
            || read(vmcs::EXIT_QUALIFICATION)? & 2 == 0
        {
            return Ok(false);
        }
        let physical = read(vmcs::GUEST_PHYSICAL_ADDRESS)?;
        let qualification = read(vmcs::EXIT_QUALIFICATION)?;
        let linear = read(vmcs::GUEST_LINEAR_ADDRESS)?;
        monitor::record_diagnostic(monitor::DiagnosticEvent::IpiFault {
            physical,
            linear,
            qualification,
            stage: boot.stage.load(Ordering::Acquire) as u64,
        });
        // A translated guest-linear access has the same low 12 bits before and
        // after paging. KVM's software MMIO/nested-EPT walk can report a GFN-
        // rounded physical fault address (paging_tmpl.h passes gfn_to_gpa to
        // kvm_translate_gpa). Use the architecturally valid linear offset for
        // register selection; native hardware yields the identical address.
        // Never use a stale/undefined GLA for a paging-structure access.
        let mut address = if qualification & 0x180 == 0x180 {
            physical & !4095 | (linear & 4095)
        } else {
            physical
        };
        monitor::write_vmcs(vmcs::VM_ENTRY_INTR_INFO_FIELD, 0)?;
        let mut decoded = None;
        if address == trap.page + 0x300 || physical == trap.page && qualification & 0x180 != 0x180 {
            let rip = read(vmcs::GUEST_RIP)?;
            let cs = read(vmcs::GUEST_CS_AR_BYTES)?;
            let base = if cs & (1 << 13) != 0 {
                0
            } else {
                read(vmcs::GUEST_CS_BASE)?
            };
            let access =
                monitor::l1_data_access().ok_or(Error::Capability("ICR instruction state", rip))?;
            let mut runtime = monitor::current_cpu().runtime.lock();
            let store = decode_store(cs, |offset| {
                let linear = base.checked_add(rip)?.checked_add(offset as u64)?;
                let physical =
                    monitor::l1_operand_physical(&runtime, linear, access, false).ok()?;
                if !runtime.allows_operand_ram(physical, 1, false) {
                    return None;
                }
                runtime.operand_byte(physical, None)
            });
            if let Some(store) = store {
                let offset = store
                    .address
                    .offset(rip.wrapping_add(u64::from(store.bytes)), |index| {
                        monitor::guest_gpr(registers, index)
                    })
                    .ok_or(Error::Capability("APIC effective address", rip))?;
                let segment = store.address.segment;
                let segment_base = if cs & (1 << 13) != 0 && segment < 4 {
                    0
                } else {
                    read(
                        [
                            vmcs::GUEST_ES_BASE,
                            vmcs::GUEST_CS_BASE,
                            vmcs::GUEST_SS_BASE,
                            vmcs::GUEST_DS_BASE,
                            vmcs::GUEST_FS_BASE,
                            vmcs::GUEST_GS_BASE,
                        ][usize::from(segment)],
                    )?
                };
                let operand = segment_base
                    .checked_add(offset)
                    .ok_or(Error::Capability("APIC linear overflow", offset))?;
                let operand = access
                    .operand_range(operand, 4)
                    .map_err(|_| Error::Capability("APIC linear range", operand))?;
                address = monitor::l1_operand_physical(&runtime, operand, access, true)
                    .map_err(|_| Error::Capability("APIC operand translation", operand))?;
                if address & !4095 != trap.page || address & 3 != 0 {
                    return Err(Error::Capability("APIC operand/fault mismatch", address));
                }
                decoded = Some((store, rip));
            }
        }
        if address == trap.page + 0x300 {
            monitor::record_diagnostic(monitor::DiagnosticEvent::IpiDecoded { x2: false });
            let (store, rip) =
                decoded.ok_or(Error::Capability("unsupported ICR store", address))?;
            let value = match store.source {
                Source::Register(index) => monitor::guest_gpr(registers, index)
                    .ok_or(Error::Capability("ICR source register", u64::from(index)))?
                    as u32,
                Source::Immediate(value) => value,
            };
            let high = monitor::current_cpu()
                .runtime
                .lock()
                .with_mmio_page(trap.page, |address| {
                    // SAFETY: checked whole UC local APIC page, naturally aligned
                    // ICR high is a side-effect-free 32-bit read, not a software copy.
                    unsafe { ((address + 0x310) as *const u32).read_volatile() }
                })
                .ok_or(Error::Capability("ICR high mapping", trap.page))?;
            let sent = deliver(
                boot,
                if high >> 24 == 255 {
                    u32::MAX
                } else {
                    high >> 24
                },
                value,
                || {
                    monitor::current_cpu()
                        .runtime
                        .lock()
                        .with_mmio_page(trap.page, |address| {
                            // SAFETY: the guest's decoded aligned 32-bit ICR store is
                            // forwarded unchanged to this CPU's checked real APIC page.
                            unsafe {
                                ((address + 0x310) as *mut u32).write_volatile(high);
                                ((address + 0x300) as *mut u32).write_volatile(value);
                            }
                        })
                        .is_some()
                },
            )?;
            if !sent {
                return Err(Error::Capability("ICR write mapping", trap.page));
            }
            monitor::advance_guest_rip(reason, 0, rip, u64::from(store.bytes), registers);
        } else {
            // Ordinary registers are not emulated. Hardware executes one guest
            // instruction; every next exit closes this per-CPU permission window.
            mtf(true)?;
            trap.writable(true)?;
            trap.stepping = true;
            monitor::record_diagnostic(monitor::DiagnosticEvent::IpiStep);
            *boot.ipi.lock() = Some(trap);
        }
        Ok(true)
    })();
    match result {
        Ok(handled) => handled,
        Err(error) => {
            use core::fmt::Write;
            let _ = writeln!(
                monitor::SerialPort,
                "thin-hv: AP IPI FAIL slot={} error={error:?}",
                boot.slot
            );
            super::fail(boot, 8)
        }
    }
}

/// Interlock only physical-destination startup IPIs. The common non-startup
/// path performs the original hardware write without a target scan or wait.
fn deliver(
    boot: &CpuBoot,
    destination: u32,
    command: u32,
    mut send: impl FnMut() -> bool,
) -> Result<bool, Error> {
    let mode = command >> 8 & 7;
    let init = mode == 5 && (command & (1 << 15) == 0 || command & (1 << 14) != 0);
    let sipi = mode == 6;
    let active = boot.stage.load(Ordering::Acquire) == super::RUNNING_L1;
    let mut targets = 0_u64;
    let mut previous = [0_usize; super::MAX_CPUS];
    if active && (init || sipi) {
        targets = startup_targets(
            &boot.handoff.ids[..boot.handoff.count],
            boot.slot,
            destination,
            command,
        )?;
        for slot in 0..boot.handoff.count {
            if init && targets & (1 << slot) != 0 {
                previous[slot] = boot
                    .cpu(slot)?
                    .boot
                    .startup_requested
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                        old.checked_add(1)
                    })
                    .map_err(|_| Error::Capability("startup generation exhausted", slot as u64))?;
            }
        }
    }
    if !send() {
        for slot in 0..boot.handoff.count {
            if init && targets & (1 << slot) != 0 {
                boot.cpu(slot)?
                    .boot
                    .startup_requested
                    .compare_exchange(
                        previous[slot] + 1,
                        previous[slot],
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .map_err(|_| Error::Capability("failed INIT rollback race", slot as u64))?;
            }
        }
        return Ok(false);
    }
    if active && (init || sipi) {
        monitor::record_diagnostic(monitor::DiagnosticEvent::IpiCommand { init, targets });
    }
    if !sipi || targets == 0 {
        return Ok(true);
    }
    let start = super::timestamp();
    let mut retried = false;
    loop {
        let mut pending = false;
        for slot in 0..boot.handoff.count {
            if targets & (1 << slot) == 0 {
                continue;
            }
            let target = &boot.cpu(slot)?.boot;
            if target.startup_completed.load(Ordering::Acquire)
                != target.startup_requested.load(Ordering::Acquire)
            {
                pending = true;
                if target.stage.load(Ordering::Acquire) >= super::FAILURE
                    || super::elapsed(boot, start)? >= 1_000_000
                {
                    return Err(Error::Capability("SIPI completion deadline", slot as u64));
                }
                super::send_ipi(
                    boot,
                    boot.handoff.ids[slot],
                    command & !(3 << 18 | 1 << 11 | 1 << 12),
                )?;
                monitor::record_diagnostic(monitor::DiagnosticEvent::IpiRetry);
                retried = true;
            }
        }
        if !pending {
            // Targeted retries must not leave a broadcast's destination or
            // shorthand changed in the physical ICR. One final original SIPI
            // restores the guest-visible ICR; all acknowledged CPUs are active
            // (or in VMX root), where duplicates are architecturally ignored.
            if retried && !send() {
                return Err(Error::Capability(
                    "restoring hardware ICR",
                    u64::from(command),
                ));
            }
            return Ok(true);
        }
        // This is a bounded delivery retry, not a longer guest boot timeout.
        // Already active targets ignore duplicate SIPIs architecturally.
        super::delay(boot, 200)?;
    }
}

/// The current interlock supports directed physical INIT/SIPI and the normal
/// all-excluding-self startup broadcast. It does not invent logical APIC IDs
/// or silently turn a self-INIT into a different CPU's reset.
fn startup_targets(
    ids: &[u32],
    sender: usize,
    destination: u32,
    command: u32,
) -> Result<u64, Error> {
    if ids.len() > 64 || sender >= ids.len() || command & (1 << 11) != 0 {
        return Err(Error::Capability(
            "startup IPI topology/destination",
            u64::from(command),
        ));
    }
    let mut targets = 0;
    for (slot, id) in ids.iter().enumerate() {
        let selected = match command >> 18 & 3 {
            0 => *id == destination || destination == u32::MAX,
            1 => slot == sender,
            2 => true,
            _ => slot != sender,
        };
        if selected {
            targets |= 1 << slot;
        }
    }
    if targets & (1 << sender) != 0 {
        return Err(Error::Capability("self startup IPI", u64::from(command)));
    }
    Ok(targets)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Source {
    Register(u8),
    Immediate(u32),
}
#[derive(Debug, Eq, PartialEq)]
struct Store {
    bytes: u8,
    source: Source,
    address: Address,
}

/// Address components of a 32-bit MOV, not a general instruction emulator.
#[derive(Debug, Eq, PartialEq)]
struct Address {
    base: Option<u8>,
    index: Option<u8>,
    scale: u8,
    displacement: u64,
    bits: u8,
    relative: bool,
    segment: u8,
}

impl Address {
    fn offset(&self, next: u64, mut register: impl FnMut(u8) -> Option<u64>) -> Option<u64> {
        let base = if self.relative {
            next
        } else if let Some(index) = self.base {
            register(index)?
        } else {
            0
        };
        let index = if let Some(index) = self.index {
            register(index)?
        } else {
            0
        };
        let value = base
            .wrapping_add(index.wrapping_shl(u32::from(self.scale)))
            .wrapping_add(self.displacement);
        Some(
            value
                & match self.bits {
                    16 => 0xffff,
                    32 => 0xffff_ffff,
                    64 => u64::MAX,
                    _ => return None,
                },
        )
    }
}

/// Resolve 32-bit MOV stores, including the effective address when a software
/// nested-EPT walk reports a page-rounded GPA with no valid GLA. Other MMIO
/// instructions continue in hardware under MTF; no device state is synthesized.
fn decode_store(cs: u64, mut fetch: impl FnMut(u8) -> Option<u8>) -> Option<Store> {
    let long = cs & (1 << 13) != 0;
    let default32 = long || cs & (1 << 14) != 0;
    let mut length = 0_u8;
    let mut byte = || {
        if length >= 15 {
            return None;
        }
        let value = fetch(length)?;
        length += 1;
        Some(value)
    };
    let (mut operand, mut address, mut rex) = (false, false, 0);
    let mut segment = None;
    let opcode = loop {
        let next = byte()?;
        match next {
            0x66 => {
                operand = true;
                rex = 0;
            }
            0x67 => {
                address = true;
                rex = 0;
            }
            0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 => {
                segment = Some(match next {
                    0x26 => 0,
                    0x2e => 1,
                    0x36 => 2,
                    0x3e => 3,
                    0x64 => 4,
                    _ => 5,
                });
                rex = 0;
            }
            0x40..=0x4f if long => rex = next,
            _ => break next,
        }
    };
    if default32 == operand || rex & 8 != 0 {
        return None;
    }
    let address_bits = if long {
        if address { 32 } else { 64 }
    } else if default32 != address {
        32
    } else {
        16
    };
    let mut memory = Address {
        base: None,
        index: None,
        scale: 0,
        displacement: 0,
        bits: address_bits,
        relative: false,
        segment: 3,
    };
    let source = if opcode == 0xa3 {
        for shift in (0..address_bits).step_by(8) {
            memory.displacement |= u64::from(byte()?) << shift;
        }
        Source::Register(0)
    } else if matches!(opcode, 0x89 | 0xc7) {
        let modrm = byte()?;
        let mode = modrm >> 6;
        let rm = modrm & 7;
        if mode == 3 || opcode == 0xc7 && modrm & 0x38 != 0 {
            return None;
        }
        let mut displacement = match mode {
            1 => 1,
            2 => {
                if address_bits == 16 {
                    2
                } else {
                    4
                }
            }
            _ => 0,
        };
        if address_bits == 16 {
            (memory.base, memory.index) = match rm {
                0 => (Some(3), Some(6)),
                1 => (Some(3), Some(7)),
                2 => (Some(5), Some(6)),
                3 => (Some(5), Some(7)),
                4 => (Some(6), None),
                5 => (Some(7), None),
                6 if mode != 0 => (Some(5), None),
                7 => (Some(3), None),
                _ => (None, None),
            };
            if mode == 0 && rm == 6 {
                displacement = 2;
            }
        } else if rm == 4 {
            let sib = byte()?;
            memory.scale = sib >> 6;
            if sib >> 3 & 7 != 4 || rex & 2 != 0 {
                memory.index = Some((sib >> 3 & 7) | (rex & 2) << 2);
            }
            if mode == 0 && sib & 7 == 5 {
                displacement = 4;
            } else {
                memory.base = Some((sib & 7) | (rex & 1) << 3);
            }
        } else if mode == 0 && rm == 5 {
            displacement = 4;
            memory.relative = long;
        } else {
            memory.base = Some(rm | (rex & 1) << 3);
        }
        if memory.base.is_some_and(|base| matches!(base & 7, 4 | 5)) {
            memory.segment = 2;
        }
        for shift in (0..displacement * 8).step_by(8) {
            memory.displacement |= u64::from(byte()?) << shift;
        }
        memory.displacement = match displacement {
            1 => (memory.displacement as i8 as i64) as u64,
            2 => (memory.displacement as i16 as i64) as u64,
            4 => (memory.displacement as i32 as i64) as u64,
            _ => 0,
        };
        if opcode == 0xc7 {
            let mut value = 0;
            for shift in [0, 8, 16, 24] {
                value |= u32::from(byte()?) << shift;
            }
            Source::Immediate(value)
        } else {
            Source::Register((modrm >> 3 & 7) | (rex & 4) << 1)
        }
    } else {
        return None;
    };
    if let Some(segment) = segment {
        memory.segment = segment;
    }
    Some(Store {
        bytes: length,
        source,
        address: memory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_ipi_selection_preserves_full_ids_and_rejects_unsupported_targets() {
        let ids = [8, 0x100, 3, 0xdeadbeef];
        assert_eq!(startup_targets(&ids, 0, 0x100, 0x4608).unwrap(), 2);
        assert_eq!(startup_targets(&ids, 2, 0xdeadbeef, 0xc500).unwrap(), 8);
        assert_eq!(startup_targets(&ids, 2, 0, 0xc4608).unwrap(), 0b1011);
        assert_eq!(startup_targets(&ids, 0, 0xfe, 0x4608).unwrap(), 0);
        for command in [0x4e08, 0x44608, 0x84608] {
            assert!(startup_targets(&ids, 0, 0x100, command).is_err());
        }
        assert!(startup_targets(&ids, 0, 8, 0xc500).is_err());
        assert!(startup_targets(&ids, 0, u32::MAX, 0x4608).is_err());
        assert!(startup_targets(&[], 0, 0, 0x4608).is_err());
        assert!(startup_targets(&ids, 4, 0, 0x4608).is_err());
        let mut boot = CpuBoot::empty();
        boot.stage = core::sync::atomic::AtomicUsize::new(super::super::RUNNING_L1);
        // Ordinary interrupts do not consult startup topology or touch hardware
        // beyond the supplied operation. Its failure remains an architectural
        // failed write, not an invented successful guest instruction.
        let mut calls = 0;
        assert!(
            deliver(&boot, 0, 0x4040, || {
                calls += 1;
                true
            })
            .unwrap()
        );
        assert_eq!(calls, 1);
        assert!(!deliver(&boot, 0, 0x4040, || false).unwrap());
    }

    #[test]
    fn icr_store_decoder_is_bounded_and_never_guesses_other_instructions() {
        for (cs, bytes, source) in [
            (1 << 13, &[0x89, 0x03][..], Source::Register(0)),
            (
                1 << 13,
                &[0x44, 0x89, 0x4c, 0x24, 0x04],
                Source::Register(9),
            ),
            (
                1 << 13,
                &[0xc7, 0x83, 0, 3, 0, 0, 6, 0x46, 0, 0],
                Source::Immediate(0x4606),
            ),
            (1 << 14, &[0xa3, 0, 3, 0xe0, 0xfe], Source::Register(0)),
            (0, &[0x66, 0x89, 0x06, 0, 3], Source::Register(0)),
        ] {
            assert_eq!(
                decode_store(cs, |i| bytes.get(usize::from(i)).copied())
                    .map(|store| (store.bytes, store.source)),
                Some((bytes.len() as u8, source))
            );
            for end in 0..bytes.len() {
                assert!(decode_store(cs, |i| bytes[..end].get(usize::from(i)).copied()).is_none());
            }
        }
        for bytes in [
            &[0x48, 0x89, 0x03][..],
            &[0x66, 0x89, 0x03],
            &[0x89, 0xc0],
            &[0xf0, 0x89, 0x03],
            &[0x87, 0x03],
            &[0xc7, 0x08],
            &[0x66; 16],
        ] {
            assert!(decode_store(1 << 13, |i| bytes.get(usize::from(i)).copied()).is_none());
        }
    }

    #[test]
    fn mov_store_addresses_cover_sib_relative_segments_and_address_size_wrap() {
        let registers = |index| {
            Some(match index {
                3 => 0xfee00000,
                4 => 0x1000,
                5 => 0xfff0,
                6 => 0x20,
                9 => 3,
                12 => 4,
                13 => 0xfee00000,
                _ => 0,
            })
        };
        for (cs, code, next, expected, segment) in [
            (1 << 13, &[0x89, 0x83, 0, 3, 0, 0][..], 0, 0xfee00300, 3),
            (
                1 << 13,
                &[0x43, 0x89, 0x84, 0x8d, 0xf4, 2, 0, 0],
                0,
                0xfee00300,
                2,
            ),
            (
                1 << 13,
                &[0x89, 0x05, 0xf0, 0xff, 0xff, 0xff],
                0xfee00310,
                0xfee00300,
                3,
            ),
            (
                1 << 13,
                &[0x67, 0x89, 0x05, 0xf0, 0xff, 0xff, 0xff],
                0x1fee00310,
                0xfee00300,
                3,
            ),
            (1 << 13, &[0x65, 0x89, 0x04, 0x25, 0, 3, 0, 0], 0, 0x300, 5),
            (0, &[0x66, 0x89, 0x02], 0, 0x10, 2),
            (1 << 14, &[0xa3, 0, 3, 0xe0, 0xfe], 0, 0xfee00300, 3),
        ] {
            let store = decode_store(cs, |i| code.get(usize::from(i)).copied()).unwrap();
            assert_eq!(store.address.offset(next, registers), Some(expected));
            assert_eq!(store.address.segment, segment);
        }
    }
}
