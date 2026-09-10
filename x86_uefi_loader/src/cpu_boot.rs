//! Firmware-time preparation for explicitly selected Direct SMP ownership.
//! Processor enumeration and reservations never start or disable firmware APs.

use super::CpuMonitor;
use super::Error;
use super::GuestRegisters;
use super::MonitorLayout;
use super::PreparedMonitor;
use core::fmt::Write;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use r_efi::efi;
use x86_64_hal::ap_bootstrap;
use x86_64_hal::ap_bootstrap::MAX_CPUS;
use x86_64_hal::cpu;
use x86_64_hal::vmcs;
use x86_64_hal::vmx;
#[path = "cpu_ipi.rs"]
mod cpu_ipi;
pub(super) use cpu_ipi::IpiTrap;

const FIRMWARE_OWNED: usize = 0;
const HANDOFF_PENDING: usize = 1;
const MONITOR_OWNED: usize = 2;
const RUNNING_L1: usize = 3;
const FAILURE: usize = 0x100;

/// Immutable topology/preparation and an atomic ownership publication. Every
/// CPU has its own object; other CPUs observe only the atomic stage. VMCSs are
/// never made current remotely, and no lock spans AP startup or guest entry.
pub(super) struct CpuBoot {
    handoff: Handoff,
    layout: Option<MonitorLayout>,
    slot: usize,
    probe: u16,
    prepared: Option<PreparedMonitor>,
    stage: AtomicUsize,
    reset_pending: AtomicUsize,
    /// Sender publishes before delivering INIT; receiver acknowledges only an
    /// actual SIPI exit, so VMX-root delivery loss cannot look like success.
    startup_requested: AtomicUsize,
    startup_completed: AtomicUsize,
    ipi: mutex::SpinLock<Option<IpiTrap>>,
}

impl CpuBoot {
    pub(super) fn identity(&self) -> Option<(usize, u32)> {
        (self.slot < self.handoff.count).then(|| (self.slot, self.handoff.ids[self.slot]))
    }

    pub(super) fn empty() -> Self {
        Self {
            handoff: Handoff {
                ids: [0; MAX_CPUS],
                count: 0,
                bootstrap: 0,
                ticks_per_us: 0,
            },
            layout: None,
            slot: 0,
            probe: 0,
            prepared: None,
            stage: AtomicUsize::new(FIRMWARE_OWNED),
            reset_pending: AtomicUsize::new(0),
            startup_requested: AtomicUsize::new(1),
            startup_completed: AtomicUsize::new(0),
            ipi: mutex::SpinLock::new(None),
        }
    }

    fn cpu(&self, slot: usize) -> Result<&'static CpuMonitor, Error> {
        let block = self
            .layout
            .and_then(|layout| layout.block(slot))
            .ok_or(Error::Capability("AP slot ownership", slot as u64))?;
        // SAFETY: prepare_aps fills every slot before BSP entry and any IPI.
        // All allocations are retained once VMX is live; each immutable record
        // is shared while mutation is confined to its locks and atomic stage.
        Ok(unsafe {
            &*((block + super::CPU_STATE_FIRST_PAGE * super::PAGE_SIZE) as *const CpuMonitor)
        })
    }
}

/// Prepare all AP maps while firmware still owns the CPUs. The only executable
/// low memory used here is the explicit RuntimeServicesCode reservation.
pub(super) fn prepare_aps(
    handoff: &super::ResidentHandoff,
    layout: MonitorLayout,
    bsp: &PreparedMonitor,
    basic: vmx::VmxBasic,
    serial: &mut super::SerialPort,
) -> Result<(), Error> {
    let topology = handoff.cpus;
    // SAFETY: the BSP object was fully initialized by prepare_monitor; no CPU
    // runs through it yet. Clone descriptor metadata, never hardware ownership.
    let ram = unsafe { bsp.monitor.as_ref() }.runtime.lock().ram.clone();
    let image = (handoff.image_base, handoff.image_base + handoff.image_size);
    let mut slots = [ap_bootstrap::Slot {
        apic_id: 0,
        stack: 0,
        argument: 0,
        host_cr3: 0,
    }; MAX_CPUS];
    for slot in 1..topology.count {
        let block = layout
            .block(slot)
            .ok_or(Error::Capability("AP reservation", slot as u64))?;
        let maps =
            super::build_carrier_maps(handoff.system_table, layout, block, image, &ram, serial)?;
        if maps.host_pat != bsp.host_pat {
            return Err(Error::Capability("AP PAT snapshot changed", maps.host_pat));
        }
        let prepared = super::prepare_monitor(
            block,
            ram.clone(),
            basic,
            [(layout.payload, layout.end), image],
            maps,
            None,
        )?;
        slots[slot - 1] = ap_bootstrap::Slot {
            apic_id: topology.ids[slot],
            stack: block + (super::GUEST_STACK_PAGE + super::GUEST_STACK_PAGES) * super::PAGE_SIZE
                - 8,
            argument: prepared.monitor.as_ptr() as usize as u64,
            host_cr3: prepared.host_cr3,
        };
        let pointer = prepared.monitor.as_ptr();
        // SAFETY: firmware still owns the AP and no Rust alias or CPU refers to
        // this newly initialized slot. Move only its preparation handle into
        // the same immovable backing object, before publishing any IPI.
        unsafe {
            (*pointer).boot = CpuBoot {
                handoff: topology,
                layout: Some(layout),
                slot,
                probe: 0,
                ipi: mutex::SpinLock::new(prepared.ipi),
                prepared: Some(prepared),
                stage: AtomicUsize::new(FIRMWARE_OWNED),
                reset_pending: AtomicUsize::new(0),
                startup_requested: AtomicUsize::new(1),
                startup_completed: AtomicUsize::new(0),
            };
        }
        // SAFETY: this complete CPU object is still unobserved by its AP. The
        // publisher borrows only its short diagnostics lock and retains no
        // reference when final bootstrap metadata is written below.
        super::publish_diagnostics(
            unsafe { &*pointer },
            block,
            block + super::MONITOR_PAGES as u64 * super::PAGE_SIZE,
            serial,
        )?;
    }
    let probe = if topology.count > 1 {
        if !ram.allows_ram_access(topology.bootstrap, 4096, true) {
            return Err(Error::OutsideIdentityMap(topology.bootstrap));
        }
        // SAFETY: the firmware-time CPL0 BSP has CPUID.VMX and EFER. These
        // read-only capabilities precede any VMXON/AP state mutation.
        let (misc, efer) = unsafe { (cpu::rdmsr(vmx::IA32_VMX_MISC), cpu::rdmsr(cpu::IA32_EFER)) };
        if misc & (1 << 8) == 0 {
            return Err(Error::Capability("carrier wait-for-SIPI", misc));
        }
        // SAFETY: with_handoff owns this aligned low RuntimeServicesCode page;
        // the checked RAM map and per-CPU platform maps cover it. No AP is
        // started yet and no alias survives this bounded preparation call.
        let output = unsafe { &mut *(topology.bootstrap as *mut [u8; 4096]) };
        ap_bootstrap::prepare(
            output,
            topology.bootstrap,
            bsp.host_cr3,
            bsp.host_pat,
            efer & !(1 << 10),
            ap_entry as *const () as usize as u64,
            &slots[..topology.count - 1],
        )
        .map_err(|_| Error::Capability("AP bootstrap layout", topology.bootstrap))?
    } else {
        0
    };
    // SAFETY: BSP is still in firmware preparation, before any private GS use
    // or hardware guest entry. No AP can observe the topology until IPIs.
    unsafe {
        (*bsp.monitor.as_ptr()).boot = CpuBoot {
            handoff: topology,
            layout: Some(layout),
            slot: 0,
            probe,
            prepared: None,
            stage: AtomicUsize::new(MONITOR_OWNED),
            reset_pending: AtomicUsize::new(0),
            startup_requested: AtomicUsize::new(1),
            startup_completed: AtomicUsize::new(1),
            ipi: mutex::SpinLock::new(bsp.ipi),
        };
    }
    for slot in 1..topology.count {
        let block = layout
            .block(slot)
            .ok_or(Error::Capability("AP final slot", slot as u64))?;
        // SAFETY: all objects are initialized but unobserved before BSP entry;
        // this final immutable probe offset is published before AP startup.
        unsafe {
            (*((block + super::CPU_STATE_FIRST_PAGE * super::PAGE_SIZE) as *mut CpuMonitor))
                .boot
                .probe = probe;
        }
    }
    cpu_ipi::prepare(topology.count, layout, bsp)?;
    let _ = writeln!(
        serial,
        "thin-hv: AP preparation PASS cpus={} ownership=firmware bootstrap={:#x} vmxon=0",
        topology.count, topology.bootstrap
    );
    Ok(())
}

/// INIT preserves PAT/cache bits; this probe performs an actual non-root entry
/// before publishing that the AP can accept the OS's later INIT/SIPI sequence.
#[derive(Clone, Copy)]
pub(super) struct InitialGuest {
    cr0: u64,
    pat: u64,
    page: u64,
    probe: u16,
}

unsafe extern "sysv64" fn ap_entry(pointer: *const CpuMonitor, cr0: u64, pat: u64) -> ! {
    // SAFETY: the immutable SIPI slot selected this full APIC identity and its
    // aligned, initialized object. Its private CR3 and reserved stack are active;
    // no firmware return exists and all backing is retained on every error.
    let monitor = unsafe { &*pointer };
    let boot = &monitor.boot;
    let Some(prepared) = boot.prepared.as_ref() else {
        fail(boot, 1)
    };
    // SAFETY: the AP is CPL0/64-bit/IF=0/CET=0 on its own mapped bootstrap
    // stack. The fresh private TSS has never been loaded, GS was bound before
    // publication, and no firmware/guest execution can return on this CPU.
    unsafe {
        prepared.environment.install_ap_bootstrap();
    }
    if boot
        .stage
        .compare_exchange(
            HANDOFF_PENDING,
            MONITOR_OWNED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        fail(boot, 2);
    }
    if let Err(error) = enter_ap(
        monitor,
        InitialGuest {
            cr0,
            pat,
            page: boot.handoff.bootstrap,
            probe: boot.probe,
        },
    ) {
        // Store a bounded classification; BSP owns the single formatted report.
        let code = match error {
            Error::Instruction(_, _, _) => 3,
            Error::Vmwrite(_, _, _) => 4,
            _ => 5,
        };
        fail(boot, code);
    }
    fail(boot, 6)
}

fn fail(boot: &CpuBoot, code: usize) -> ! {
    boot.stage.store(FAILURE + code, Ordering::Release);
    loop {
        core::hint::spin_loop();
    }
}

fn enter_ap(monitor: &CpuMonitor, guest: InitialGuest) -> Result<(), Error> {
    let prepared = monitor
        .boot
        .prepared
        .as_ref()
        .ok_or(Error::Capability("AP preparation missing", 0))?;
    if cpu::cpuid(1, 0).ecx & ((1 << 5) | (1 << 26)) != (1 << 5) | (1 << 26) {
        return Err(Error::Capability("AP VMX/XSAVE", 0));
    }
    // SAFETY: this AP has private GS/IDT at CPL0 and CPUID established VMX.
    // Compare its actual capability snapshot before using BSP-prepared state.
    let (basic, limits, feature) = unsafe {
        (
            vmx::VmxBasic::from_msr(cpu::rdmsr(vmx::IA32_VMX_BASIC)),
            super::capture_host_validation_limits(monitor.physical_bits),
            cpu::rdmsr(cpu::IA32_FEATURE_CONTROL),
        )
    };
    if limits != monitor.host_limits
        || basic != monitor.runtime.lock().basic
        || feature & 1 != 0 && feature & 4 == 0
    {
        return Err(Error::Capability("AP VMX capabilities differ", feature));
    }
    let host_cr0 = (cpu::read_cr0() | limits.cr0_fixed0) & limits.cr0_fixed1;
    let host_cr4 = (cpu::read_cr4()
        | limits.cr4_fixed0
        | super::CR4_VMX_ENABLE
        | super::CR4_OSXSAVE
        | (1 << 9))
        & limits.cr4_fixed1;
    if host_cr4 & (super::CR4_VMX_ENABLE | super::CR4_OSXSAVE)
        != super::CR4_VMX_ENABLE | super::CR4_OSXSAVE
    {
        return Err(Error::Capability("AP host CR4", host_cr4));
    }
    let block = monitor.runtime.lock().cpu_block;
    let vmxon = super::VmxonPhys::new(block).ok_or(Error::OutsideIdentityMap(block))?;
    let carrier = super::VmcsPhys::new(block + 4096).ok_or(Error::OutsideIdentityMap(block))?;
    // SAFETY: fixed-bit checks and CPUID established these controls; the WB
    // aligned VMXON page has this AP's revision and no other CPU owns it. From
    // here all failures retain this AP's private environment and allocations.
    unsafe {
        if feature & 1 == 0 {
            cpu::wrmsr(cpu::IA32_FEATURE_CONTROL, feature | 5);
        }
        cpu::write_cr0(host_cr0);
        cpu::write_cr4(host_cr4);
        super::require("AP VMXON", vmx::vmxon(vmxon))?;
    }
    let result = super::configure_and_launch(
        carrier,
        prepared.eptp,
        block + super::MSR_BITMAP_PAGE * 4096,
        host_cr0,
        guest.cr0,
        super::CR4_VMX_ENABLE,
        0,
        host_cr4,
        prepared.host_cr3,
        prepared.host_pat,
        &prepared.environment,
        0,
        0,
        basic.true_controls,
        Some(guest),
    );
    // SAFETY: only failed carrier preparation/entry returns, still on this AP
    // in VMX root. No allocation is freed even if VMXOFF itself fails.
    let _ = unsafe { vmx::vmxoff() };
    result
}

pub(super) fn write_initial_guest(guest: InitialGuest) -> Result<(), Error> {
    write_reset_state(guest.cr0)?;
    for (field, value) in [
        (vmcs::GUEST_IA32_PAT, guest.pat),
        (vmcs::GUEST_CS_SELECTOR, guest.page >> 4),
        (vmcs::GUEST_CS_BASE, guest.page),
        (vmcs::GUEST_RIP, u64::from(guest.probe)),
        (vmcs::GUEST_ACTIVITY_STATE, 0),
    ] {
        super::write_vmcs(field, value)?;
    }
    Ok(())
}

/// Intel INIT register state, not a saved pre-L2 software snapshot. PAT,
/// XCR0/XSS/XSAVE, MXCSR and ordinary MSRs are deliberately left unchanged.
fn write_reset_state(old_cr0: u64) -> Result<(), Error> {
    let cr0 = (old_cr0 & ((1 << 29) | (1 << 30))) | (1 << 4);
    for (selector, base, limit, access, code) in [
        (
            vmcs::GUEST_CS_SELECTOR,
            vmcs::GUEST_CS_BASE,
            vmcs::GUEST_CS_LIMIT,
            vmcs::GUEST_CS_AR_BYTES,
            true,
        ),
        (
            vmcs::GUEST_SS_SELECTOR,
            vmcs::GUEST_SS_BASE,
            vmcs::GUEST_SS_LIMIT,
            vmcs::GUEST_SS_AR_BYTES,
            false,
        ),
        (
            vmcs::GUEST_DS_SELECTOR,
            vmcs::GUEST_DS_BASE,
            vmcs::GUEST_DS_LIMIT,
            vmcs::GUEST_DS_AR_BYTES,
            false,
        ),
        (
            vmcs::GUEST_ES_SELECTOR,
            vmcs::GUEST_ES_BASE,
            vmcs::GUEST_ES_LIMIT,
            vmcs::GUEST_ES_AR_BYTES,
            false,
        ),
        (
            vmcs::GUEST_FS_SELECTOR,
            vmcs::GUEST_FS_BASE,
            vmcs::GUEST_FS_LIMIT,
            vmcs::GUEST_FS_AR_BYTES,
            false,
        ),
        (
            vmcs::GUEST_GS_SELECTOR,
            vmcs::GUEST_GS_BASE,
            vmcs::GUEST_GS_LIMIT,
            vmcs::GUEST_GS_AR_BYTES,
            false,
        ),
    ] {
        for (field, value) in [
            (selector, if code { 0xf000 } else { 0 }),
            (base, if code { 0xffff_0000 } else { 0 }),
            (limit, 0xffff),
            (access, 0x93),
        ] {
            super::write_vmcs(field, value)?;
        }
    }
    for (field, value) in [
        (vmcs::CR0_READ_SHADOW, cr0),
        (vmcs::CR0_GUEST_HOST_MASK, super::control_state::CR0_NE),
        (vmcs::CR4_READ_SHADOW, 0),
        (vmcs::CR4_GUEST_HOST_MASK, super::CR4_VMX_ENABLE),
        (vmcs::GUEST_CR0, cr0 | super::control_state::CR0_NE),
        (vmcs::GUEST_CR3, 0),
        (vmcs::GUEST_CR4, super::CR4_VMX_ENABLE),
        (vmcs::GUEST_IA32_EFER, 0),
        (vmcs::GUEST_LDTR_SELECTOR, 0),
        (vmcs::GUEST_LDTR_BASE, 0),
        (vmcs::GUEST_LDTR_LIMIT, 0xffff),
        (vmcs::GUEST_LDTR_AR_BYTES, 0x82),
        (vmcs::GUEST_TR_SELECTOR, 0),
        (vmcs::GUEST_TR_BASE, 0),
        (vmcs::GUEST_TR_LIMIT, 0xffff),
        (vmcs::GUEST_TR_AR_BYTES, 0x8b),
        (vmcs::GUEST_GDTR_BASE, 0),
        (vmcs::GUEST_GDTR_LIMIT, 0xffff),
        (vmcs::GUEST_IDTR_BASE, 0),
        (vmcs::GUEST_IDTR_LIMIT, 0xffff),
        (vmcs::GUEST_DR7, 0x400),
        (vmcs::GUEST_RIP, 0xfff0),
        (vmcs::GUEST_RSP, 0),
        (vmcs::GUEST_RFLAGS, 2),
        (vmcs::GUEST_PENDING_DBG_EXCEPTIONS, 0),
        (vmcs::GUEST_INTERRUPTIBILITY_INFO, 0),
        (vmcs::VM_ENTRY_INTR_INFO_FIELD, 0),
        (vmcs::GUEST_ACTIVITY_STATE, 3),
    ] {
        super::write_vmcs(field, value)?;
    }
    // SAFETY: this CPU owns the current carrier in VMX root; only IA32e guest
    // mode changes, leaving the already checked PAT/EFER load controls intact.
    let controls = unsafe { vmx::vmread(vmcs::VM_ENTRY_CONTROLS) }
        .map_err(|status| Error::Instruction("AP entry controls", status, 0))?;
    super::write_vmcs(
        vmcs::VM_ENTRY_CONTROLS,
        controls & !u64::from(vmcs::VM_ENTRY_IA32E_MODE),
    )
}

/// Consumes only the monitor's initial carrier probe and architectural INIT/
/// SIPI exits. Every ordinary L1 CPUID continues through the common dispatcher.
pub(super) fn handle_exit(registers: &mut GuestRegisters, reason: u64) -> bool {
    let monitor = super::current_cpu();
    let boot = &monitor.boot;
    if reason & (1 << 31) != 0 {
        return false;
    }
    let reason = reason & 0xffff;
    if cpu_ipi::handle_exit(registers, reason) {
        return true;
    }
    let initial_probe = reason == super::EXIT_REASON_CPUID
        && registers.rax as u32 == ap_bootstrap::READY_LEAF
        && boot.slot != 0
        && boot.stage.load(Ordering::Acquire) == MONITOR_OWNED;
    if !initial_probe && reason != 3 && reason != 4 {
        return false;
    }
    if reason == 3 {
        super::record_diagnostic(super::DiagnosticEvent::IpiEpoch {
            requested: boot.startup_requested.load(Ordering::Acquire) as u64,
            completed: boot.startup_completed.load(Ordering::Acquire) as u64,
            accepted: false,
        });
        // Actually enter WAIT before the sender's SIPI, rather than spending
        // its short INIT/SIPI interval issuing a full register reset in root.
        // No L1 instruction can execute in WAIT. Materialize the reset on the
        // retained SIPI, before making the carrier active again.
        boot.reset_pending.store(1, Ordering::Relaxed);
        for (field, value) in [
            (vmcs::GUEST_ACTIVITY_STATE, 3),
            (vmcs::VM_ENTRY_INTR_INFO_FIELD, 0),
            (vmcs::GUEST_INTERRUPTIBILITY_INFO, 0),
        ] {
            if super::write_vmcs(field, value).is_err() {
                fail(boot, 7);
            }
        }
        return true;
    }
    let reset = initial_probe || boot.reset_pending.swap(0, Ordering::Relaxed) != 0;
    let mut accepted_sipi = false;
    // SAFETY: hardware just exited this owner's carrier; no L1 runs while its
    // INIT/SIPI state is read or rewritten, and private GS selects this object.
    let result = unsafe {
        if reason == 4 {
            vmx::vmread(vmcs::EXIT_QUALIFICATION)
                .map_err(|s| Error::Instruction("SIPI vector", s, 0))
                .and_then(|qualification| {
                    let vector = qualification & 255;
                    // A second project SIPI can race the first probe. The OS
                    // cannot allocate our firmware-reserved bootstrap page.
                    if vector == boot.handoff.bootstrap >> 12 {
                        if reset {
                            boot.reset_pending.store(1, Ordering::Relaxed);
                        }
                        return super::write_vmcs(vmcs::GUEST_ACTIVITY_STATE, 3);
                    }
                    if reset {
                        let cr0 = vmx::vmread(vmcs::GUEST_CR0)
                            .map_err(|s| Error::Instruction("SIPI reset CR0", s, 0))?;
                        write_reset_state(cr0)?;
                    }
                    for (field, value) in [
                        (vmcs::GUEST_CS_SELECTOR, vector << 8),
                        (vmcs::GUEST_CS_BASE, vector << 12),
                        (vmcs::GUEST_RIP, 0),
                        (vmcs::GUEST_ACTIVITY_STATE, 0),
                    ] {
                        super::write_vmcs(field, value)?;
                    }
                    accepted_sipi = true;
                    Ok(())
                })
        } else {
            // Publish WAIT immediately: SIPI received while active is discarded
            // architecturally. Remaining INIT fields are prepared before entry.
            super::write_vmcs(vmcs::GUEST_ACTIVITY_STATE, 3).and_then(|()| {
                let cr0 = vmx::vmread(vmcs::GUEST_CR0)
                    .map_err(|s| Error::Instruction("INIT CR0", s, 0))?;
                write_reset_state(cr0)
            })
        }
    };
    if result.is_err() {
        fail(boot, 7);
    }
    if reset {
        *registers = GuestRegisters {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: u64::from(cpu::cpuid(1, 0).eax),
            rbp: 0,
            rsi: 0,
            rdi: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
        };
        // SAFETY: VM exit cleared host DR7.GD; CPL0 owns these registers. INIT
        // resets CR2/DR0-3/DR6 without changing the saved guest extended state.
        unsafe {
            core::arch::asm!("xor eax, eax", "mov cr2, rax", "mov dr0, rax", "mov dr1, rax",
            "mov dr2, rax", "mov dr3, rax", "mov eax, 0xffff0ff0", "mov dr6, rax",
            out("rax") _, options(nostack));
        }
    }
    if initial_probe {
        if cpu_ipi::activate(boot).is_err() {
            fail(boot, 8);
        }
        boot.stage.store(RUNNING_L1, Ordering::Release);
    } else if accepted_sipi {
        boot.startup_completed.store(
            boot.startup_requested.load(Ordering::Acquire),
            Ordering::Release,
        );
        super::record_diagnostic(super::DiagnosticEvent::IpiEpoch {
            requested: boot.startup_requested.load(Ordering::Acquire) as u64,
            completed: boot.startup_completed.load(Ordering::Acquire) as u64,
            accepted: true,
        });
    }
    true
}

/// All APs must have exited their own carrier probe before BSP continues the
/// original successful ExitBootServices call. No firmware calls remain here.
pub(super) fn take_over_aps() -> Result<(), Error> {
    let bsp = super::current_cpu();
    let boot = &bsp.boot;
    if boot.slot != 0 {
        return Err(Error::Capability("EBS on non-BSP", boot.slot as u64));
    }
    if boot.handoff.count > 1 {
        for slot in 1..boot.handoff.count {
            boot.cpu(slot)?
                .boot
                .stage
                .compare_exchange(
                    FIRMWARE_OWNED,
                    HANDOFF_PENDING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .map_err(|stage| Error::Capability("AP ownership transition", stage as u64))?;
            send_ipi(boot, boot.handoff.ids[slot], 0xc500)?;
        }
        delay(boot, 10_000)?;
        for slot in 1..boot.handoff.count {
            send_ipi(
                boot,
                boot.handoff.ids[slot],
                0x4600 | (boot.handoff.bootstrap >> 12) as u32,
            )?;
        }
        delay(boot, 200)?;
        for slot in 1..boot.handoff.count {
            if boot.cpu(slot)?.boot.stage.load(Ordering::Acquire) < RUNNING_L1 {
                send_ipi(
                    boot,
                    boot.handoff.ids[slot],
                    0x4600 | (boot.handoff.bootstrap >> 12) as u32,
                )?;
            }
        }
        let start = timestamp();
        loop {
            let mut ready = true;
            for slot in 1..boot.handoff.count {
                let stage = boot.cpu(slot)?.boot.stage.load(Ordering::Acquire);
                if stage >= FAILURE {
                    return Err(Error::Capability(
                        "AP bootstrap failed",
                        ((slot as u64) << 32) | stage as u64,
                    ));
                }
                ready &= stage == RUNNING_L1;
            }
            if ready {
                break;
            }
            if elapsed(boot, start)? > 5_000_000 {
                for slot in 1..boot.handoff.count {
                    let _ = writeln!(
                        super::SerialPort,
                        "thin-hv: AP barrier slot={} apic_id={} state={}",
                        slot,
                        boot.handoff.ids[slot],
                        boot.cpu(slot)?.boot.stage.load(Ordering::Acquire)
                    );
                }
                return Err(Error::Capability(
                    "AP startup deadline",
                    boot.handoff.count as u64,
                ));
            }
            core::hint::spin_loop();
        }
    }
    cpu_ipi::activate(boot)?;
    boot.stage.store(RUNNING_L1, Ordering::Release);
    for slot in 0..boot.handoff.count {
        let cpu = boot.cpu(slot)?;
        let _ = writeln!(
            super::SerialPort,
            "thin-hv: CPU carrier PASS slot={} apic_id={} vmcs={:#x} ownership=monitor",
            slot,
            boot.handoff.ids[slot],
            cpu.runtime.lock().cpu_block + 4096
        );
    }
    let _ = writeln!(
        super::SerialPort,
        "thin-hv: firmware handoff PASS exit_boot_services=success cpus={} ap_takeover={}",
        boot.handoff.count,
        boot.handoff.count.saturating_sub(1)
    );
    Ok(())
}

fn elapsed(boot: &CpuBoot, start: u64) -> Result<u64, Error> {
    timestamp()
        .checked_sub(start)
        .and_then(|ticks| ticks.checked_div(boot.handoff.ticks_per_us))
        .ok_or(Error::Capability("AP local clock", 0))
}

fn delay(boot: &CpuBoot, micros: u64) -> Result<(), Error> {
    let start = timestamp();
    while elapsed(boot, start)? < micros {
        core::hint::spin_loop();
    }
    Ok(())
}

fn send_ipi(boot: &CpuBoot, id: u32, command: u32) -> Result<(), Error> {
    // SAFETY: CPUID.APIC was checked before reserving the bootstrap. Read the
    // actual current mode/base at CPL0 rather than assuming a q35 MMIO address.
    let apic = unsafe { cpu::rdmsr(0x1b) };
    if apic & (1 << 11) == 0 {
        return Err(Error::Capability("APIC disabled at EBS", apic));
    }
    if apic & (1 << 10) != 0 {
        // SAFETY: enabled x2APIC establishes writable ICR MSR. The destination
        // came from the checked firmware inventory, and commands are only the
        // targeted INIT/SIPI sequences following successful ExitBootServices.
        unsafe {
            cpu::wrmsr(0x830, (u64::from(id) << 32) | u64::from(command));
        }
        return Ok(());
    }
    if id >= 255 {
        return Err(Error::Capability("xAPIC destination", u64::from(id)));
    }
    let base = apic & 0x000f_ffff_ffff_f000;
    let start = timestamp();
    let result = super::current_cpu()
        .runtime
        .lock()
        .with_mmio_page(base, |address| {
            // SAFETY: with_mmio_page validates this complete UC MMIO page. The live
            // IA32_APIC_BASE identifies local APIC registers, accessed only as
            // aligned 32-bit words on the BSP with IF clear. No guest runs here.
            unsafe {
                while core::ptr::read_volatile((address + 0x300) as *const u32) & (1 << 12) != 0 {
                    if elapsed(boot, start)? > 100_000 {
                        return Err(Error::Capability("xAPIC ICR busy", u64::from(id)));
                    }
                    core::hint::spin_loop();
                }
                core::ptr::write_volatile((address + 0x310) as *mut u32, id << 24);
                core::ptr::write_volatile((address + 0x300) as *mut u32, command);
            }
            Ok(())
        });
    result.ok_or(Error::OutsideIdentityMap(base))?
}

/// Copied into private monitor storage before Boot Services can disappear.
/// Slot zero is the actual BSP, not an assumption that its APIC ID is zero.
#[derive(Clone, Copy)]
pub(super) struct Handoff {
    pub(super) ids: [u32; MAX_CPUS],
    pub(super) count: usize,
    pub(super) bootstrap: u64,
    pub(super) ticks_per_us: u64,
}

impl Handoff {
    pub(super) fn allocation_pages(&self) -> usize {
        // read() checked this count against the same fixed CPU capacity.
        self.count * super::MONITOR_PAGES + 2
    }
}

/// Uses the firmware reservation allocator, never an assumed free low address.
/// Failed pre-entry preparation frees this page through the common owner.
pub(super) fn with_handoff<T>(
    system: *mut efi::SystemTable,
    prepare: impl FnOnce(&Handoff) -> Result<T, Error>,
) -> Result<T, Error> {
    let inventory = crate::cpu_inventory::read(system).map_err(Error::Platform)?;
    let processors = inventory.processors();
    let required = efi::protocols::mp_services::PROCESSOR_ENABLED_BIT
        | efi::protocols::mp_services::PROCESSOR_HEALTH_STATUS_BIT;
    if inventory.enabled != inventory.total
        || processors
            .iter()
            .any(|cpu| cpu.flags & required != required)
    {
        return Err(Error::Capability(
            "SMP requires healthy enabled firmware CPUs",
            inventory.total as u64,
        ));
    }
    let mut handoff = Handoff {
        ids: [0; MAX_CPUS],
        count: inventory.total,
        bootstrap: 0,
        ticks_per_us: 0,
    };
    handoff.ids[0] = processors[inventory.bsp].apic_id;
    let mut slot = 1;
    for (index, processor) in processors.iter().enumerate() {
        if index != inventory.bsp {
            handoff.ids[slot] = processor.apic_id;
            slot += 1;
        }
    }
    if handoff.count == 1 {
        return prepare(&handoff);
    }
    if cpu::cpuid(1, 0).edx & ((1 << 9) | (1 << 4)) != (1 << 9) | (1 << 4) {
        return Err(Error::Capability("AP startup requires local APIC", 0));
    }
    // SAFETY: this firmware-time BSP is CPL0; CPUID established local APIC and
    // therefore IA32_APIC_BASE. No AP ownership or APIC state changes here.
    let apic = unsafe { cpu::rdmsr(0x1b) };
    if apic & (1 << 11) == 0
        || handoff.ids[..handoff.count].contains(&u32::MAX)
        || (apic & (1 << 10) == 0 && handoff.ids[..handoff.count].iter().any(|&id| id >= 255))
    {
        return Err(Error::Capability("firmware APIC startup addressing", apic));
    }
    handoff.ticks_per_us = calibrate_tsc(system)?;
    super::with_runtime_pages(system, efi::RUNTIME_SERVICES_CODE, 1, 1 << 20, |page| {
        handoff.bootstrap = page;
        prepare(&handoff)
    })
}

/// L0 startup deadlines use local elapsed TSC, never another CPU's start time.
/// CPUID's crystal ratio is preferred; otherwise calibrate before EBS. No
/// firmware timer or filesystem protocol is needed during AP ownership transfer.
fn calibrate_tsc(system: *mut efi::SystemTable) -> Result<u64, Error> {
    let ratio = if cpu::cpuid(0, 0).eax >= 0x15 {
        let leaf = cpu::cpuid(0x15, 0);
        if leaf.eax != 0 && leaf.ebx != 0 && leaf.ecx != 0 {
            Some(
                (u64::from(leaf.ecx) * u64::from(leaf.ebx))
                    .div_ceil(u64::from(leaf.eax) * 1_000_000),
            )
        } else {
            None
        }
    } else {
        None
    };
    let ticks = if let Some(ticks) = ratio {
        ticks
    } else {
        let services = crate::chainload::boot_services(system).map_err(Error::Platform)?;
        let _ = cpu::cpuid(0, 0);
        let before = timestamp();
        // SAFETY: checked live Boot Services; this bounded synchronous delay
        // occurs before VMX or AP startup and borrows no firmware buffer.
        let status = unsafe { ((*services).stall)(10_000) };
        if status.is_error() {
            return Err(Error::Firmware(
                "AP clock firmware Stall",
                status.as_usize(),
            ));
        }
        let _ = cpu::cpuid(0, 0);
        let after = timestamp();
        after
            .checked_sub(before)
            .ok_or(Error::Capability("AP clock moved backwards", 0))?
            .div_ceil(10_000)
    };
    if !(1..=10_000).contains(&ticks) {
        return Err(Error::Capability("AP clock calibration range", ticks));
    }
    Ok(ticks)
}

fn timestamp() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: called only at CPL0 after CPUID.TSC was established. Intel x86-64
    // provides LFENCE; neither instruction accesses a pointer or changes state.
    unsafe {
        core::arch::asm!("lfence", "rdtsc", out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
    }
    (u64::from(high) << 32) | u64::from(low)
}
