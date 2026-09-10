//! Monitor-owned long-mode descriptor tables and bounded exception stacks.
//!
//! These tables are prepared before VM entry; this module never executes LGDT,
//! LIDT, LTR, or a VMX instruction. VMCS host fields select them on VM exit.
//! The backing storage must remain reserved at the same linear addresses while
//! any CPU or VMCS can reference it. Each physical CPU needs its own instance.
//!
//! Every unexpected exception stops the monitor, including a root-mode NMI.
//! Only an explicitly armed RDMSR #GP(0) has a bounded recovery continuation.
//! This is a visible fail-closed exception environment, not an NMI
//! forwarding implementation or evidence of physical-machine daily-use support.
//! No heap, firmware service, lock, stack unwinding, or XSAVE state is used by
//! the exception entry. Host CR4.CET must be clear: shadow stacks are not owned
//! by this module. FS-based TLS is not part of the host ABI; GS addresses the
//! owning CPU's legacy extended-state scratch, not firmware or guest TLS.
//!
//! Layouts follow Intel SDM Volume 3A, "64-Bit Mode TSS" and "64-Bit IDT Gate
//! Descriptors", and Volume 3C, "Checks on Host Segment and Descriptor-Table
//! Registers" / "Loading Host Segment and Descriptor-Table Registers".
//! The latter requires RPL=TI=0 and non-null CS/TR, and sets GDTR/IDTR limits to
//! 0xffff on VM exit. Only the fixed GDT selectors below may be used: the actual
//! allocation is a page, not an implied 64-KiB GDT. All 256 IDT gates are present.
//!
//! Primary references: <https://cdrdv2-public.intel.com/868137/325462-089-sdm-vol-1-2abcd-3abcd-4.pdf>
//! and <https://cdrdv2-public.intel.com/868136/252046-081-sdm-change-document.pdf>.

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;
use core::sync::atomic::compiler_fence;

use crate::vmcs;

/// Size and alignment of each reserved host-environment page.
pub const HOST_PAGE_BYTES: usize = 4096;
/// Two table pages followed by four independent, two-page IST stacks.
pub const HOST_ENVIRONMENT_PAGES: usize = 10;
/// Exact caller-owned storage size required by [`HostEnvironment::initialize`].
pub const HOST_ENVIRONMENT_BYTES: usize = HOST_ENVIRONMENT_PAGES * HOST_PAGE_BYTES;
/// Private execute/read, ring-zero, 64-bit code selector.
pub const HOST_CODE_SELECTOR: u16 = 8;
/// Private read/write, ring-zero data selector.
pub const HOST_DATA_SELECTOR: u16 = 16;
/// Private busy 64-bit TSS selector; its descriptor occupies two GDT entries.
pub const HOST_TSS_SELECTOR: u16 = 24;

const TSS_OFFSET: usize = 64;
const TSS_BYTES: usize = 104;
const XSTATE_OFFSET: usize = 256;
/// GS-relative offset of the monitor's masked, round-to-nearest MXCSR value.
pub const HOST_XSTATE_MXCSR_OFFSET: usize = core::mem::offset_of!(HostXstate, monitor_mxcsr);

/// Per-CPU save area for L0's baseline x87/SSE register use. This is not an
/// L1/L2 context switch: each exit saves the live context and restores that same
/// context, including on reflection. L0 must not execute AVX or touch other
/// XSAVE components without extending its preservation contract.
#[repr(C, align(64))]
struct HostXstate {
    guest_fx: [u8; 512],
    monitor_mxcsr: u32,
    msr_fault_rip: u64,
    msr_resume_rip: u64,
    msr_faulted: u64,
    monitor_data: usize,
}
const MSR_FAULT_RIP: usize = core::mem::offset_of!(HostXstate, msr_fault_rip);
const MSR_RESUME_RIP: usize = core::mem::offset_of!(HostXstate, msr_resume_rip);
const MSR_FAULTED: usize = core::mem::offset_of!(HostXstate, msr_faulted);
const MONITOR_DATA: usize = core::mem::offset_of!(HostXstate, monitor_data);
const IDT_OFFSET: usize = HOST_PAGE_BYTES;
const IDT_GATE_BYTES: usize = 16;
const IDT_ENTRIES: usize = 256;
const IST_COUNT: usize = 4;
const IST_STACK_BYTES: usize = 2 * HOST_PAGE_BYTES;
const IST_STACKS_OFFSET: usize = 2 * HOST_PAGE_BYTES;
// Accessed bits are already set, so interrupt entry need not modify the GDT.
const CODE_DESCRIPTOR: u64 = 0x00af_9b00_0000_ffff;
const DATA_DESCRIPTOR: u64 = 0x00cf_9300_0000_ffff;
const EXCEPTION_MESSAGE: &[u8] = b"thin-hv: host exception FAIL: stopped\r\n";
static EXCEPTION_MESSAGE_BYTES: [u8; EXCEPTION_MESSAGE.len()] =
    *b"thin-hv: host exception FAIL: stopped\r\n";

const _: () = assert!(TSS_OFFSET + TSS_BYTES <= XSTATE_OFFSET);
const _: () = assert!(XSTATE_OFFSET % core::mem::align_of::<HostXstate>() == 0);
const _: () = assert!(XSTATE_OFFSET + core::mem::size_of::<HostXstate>() <= HOST_PAGE_BYTES);
const _: () = assert!(core::mem::offset_of!(HostXstate, guest_fx) == 0);
const _: () = assert!(HOST_XSTATE_MXCSR_OFFSET == 512);
const _: () = assert!(IDT_GATE_BYTES * IDT_ENTRIES == HOST_PAGE_BYTES);
const _: () = assert!(IST_STACKS_OFFSET + IST_COUNT * IST_STACK_BYTES == HOST_ENVIRONMENT_BYTES);

/// A rejected host layout; initialization performs no writes on these errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Only architectural 48-bit and 57-bit canonical addresses are accepted.
    LinearAddressWidth,
    /// Backing storage must contain exactly [`HOST_ENVIRONMENT_BYTES`] bytes.
    StorageSize,
    /// Table and stack ranges must have page-aligned bases and lengths.
    Alignment,
    /// A stack is empty or smaller than one page, or a range starts at zero.
    EmptyRange,
    /// A range's exclusive end cannot be represented without wrapping.
    Overflow,
    /// A range, stack top, or handler pointer is not canonical for this host.
    Noncanonical,
    /// The ordinary host stack overlaps the descriptor/exception-stack block.
    Overlap,
    /// Initializing writable storage would overwrite the exception code.
    HandlerOverlap,
    /// An IDT gate requested an IST index outside the architectural range 1..=7.
    IstIndex,
}

/// An owned, downward-growing host stack described without dereferencing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostStack {
    base: u64,
    end: u64,
}

impl HostStack {
    /// Validates page alignment, minimum capacity and checked end arithmetic.
    ///
    /// Canonicality is checked against the host's linear-address mode when the
    /// stack is bound to an environment. The caller retains memory ownership.
    pub fn new(base: u64, bytes: u64) -> Result<Self, Error> {
        if base == 0 || bytes < HOST_PAGE_BYTES as u64 {
            return Err(Error::EmptyRange);
        }
        if base & (HOST_PAGE_BYTES as u64 - 1) != 0 || bytes & (HOST_PAGE_BYTES as u64 - 1) != 0 {
            return Err(Error::Alignment);
        }
        let end = base.checked_add(bytes).ok_or(Error::Overflow)?;
        Ok(Self { base, end })
    }

    /// Returns the half-open range which must remain writable in HOST_CR3.
    #[must_use]
    pub const fn range(self) -> (u64, u64) {
        (self.base, self.end)
    }

    /// Returns the aligned, exclusive stack top used by the TSS.
    #[must_use]
    pub const fn top(self) -> u64 {
        self.end
    }

    /// Matches the existing SysV VM-exit entry's initial RSP alignment.
    #[must_use]
    pub const fn vmexit_rsp(self) -> u64 {
        self.end - 8
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Layout {
    base: u64,
    end: u64,
    host_stack: HostStack,
    exception_entry: u64,
    ist_tops: [u64; IST_COUNT],
}

impl Layout {
    fn new(
        base: u64,
        bytes: usize,
        host_stack: HostStack,
        linear_bits: u8,
        exception_entry: u64,
    ) -> Result<Self, Error> {
        if !matches!(linear_bits, 48 | 57) {
            return Err(Error::LinearAddressWidth);
        }
        if bytes != HOST_ENVIRONMENT_BYTES {
            return Err(Error::StorageSize);
        }
        if base == 0 {
            return Err(Error::EmptyRange);
        }
        if base & (HOST_PAGE_BYTES as u64 - 1) != 0 {
            return Err(Error::Alignment);
        }
        let end = base
            .checked_add(HOST_ENVIRONMENT_BYTES as u64)
            .ok_or(Error::Overflow)?;
        check_range(base, end, linear_bits)?;
        check_range(host_stack.base, host_stack.end, linear_bits)?;
        // IST uses an exclusive end directly as RSP, not end-1. Check both tops.
        if !canonical(end, linear_bits)
            || !canonical(host_stack.end, linear_bits)
            || exception_entry == 0
            || !canonical(exception_entry, linear_bits)
        {
            return Err(Error::Noncanonical);
        }
        if base < host_stack.end && host_stack.base < end {
            return Err(Error::Overlap);
        }
        if (base..end).contains(&exception_entry)
            || (host_stack.base..host_stack.end).contains(&exception_entry)
        {
            return Err(Error::HandlerOverlap);
        }
        let mut ist_tops = [0; IST_COUNT];
        for (index, top) in ist_tops.iter_mut().enumerate() {
            // The checked total block length covers each strictly interior sum.
            *top = base + (IST_STACKS_OFFSET + (index + 1) * IST_STACK_BYTES) as u64;
        }
        Ok(Self {
            base,
            end,
            host_stack,
            exception_entry,
            ist_tops,
        })
    }
}

fn canonical(address: u64, bits: u8) -> bool {
    let shift = 64 - bits;
    (((address << shift) as i64) >> shift) as u64 == address
}

fn check_range(base: u64, end: u64, bits: u8) -> Result<(), Error> {
    if base >= end {
        return Err(Error::EmptyRange);
    }
    let last = end - 1;
    let sign = 1_u64 << (bits - 1);
    if !canonical(base, bits) || !canonical(last, bits) || (base ^ last) & sign != 0 {
        return Err(Error::Noncanonical);
    }
    Ok(())
}

fn tss_descriptor(base: u64) -> [u64; 2] {
    let low = (TSS_BYTES as u64 - 1)
        | ((base & 0xffff) << 16)
        | (((base >> 16) & 0xff) << 32)
        | (0x8b_u64 << 40)
        | (((base >> 24) & 0xff) << 56);
    [low, base >> 32]
}

fn idt_gate(entry: u64, ist: u8, linear_bits: u8) -> Result<[u8; IDT_GATE_BYTES], Error> {
    if !matches!(linear_bits, 48 | 57) {
        return Err(Error::LinearAddressWidth);
    }
    if entry == 0 || !canonical(entry, linear_bits) {
        return Err(Error::Noncanonical);
    }
    if !(1..=7).contains(&ist) {
        return Err(Error::IstIndex);
    }
    let mut bytes = [0; IDT_GATE_BYTES];
    bytes[..2].copy_from_slice(&(entry as u16).to_le_bytes());
    bytes[2..4].copy_from_slice(&HOST_CODE_SELECTOR.to_le_bytes());
    bytes[4] = ist;
    bytes[5] = 0x8e; // Present, DPL0, 64-bit interrupt gate; reserved bits clear.
    bytes[6..8].copy_from_slice(&((entry >> 16) as u16).to_le_bytes());
    bytes[8..12].copy_from_slice(&((entry >> 32) as u32).to_le_bytes());
    Ok(bytes)
}

const fn vector_ist(vector: usize) -> u8 {
    match vector {
        8 => 1,  // Double fault must not reuse an already-failed ordinary stack.
        2 => 2,  // NMI can arrive while another stop handler is running.
        18 => 3, // Machine check has an independent emergency stack.
        _ => 4,
    }
}

fn populate(storage: &mut [u8], layout: Layout, gates: [[u8; IDT_GATE_BYTES]; IST_COUNT]) {
    storage.fill(0);
    let mxcsr = XSTATE_OFFSET + HOST_XSTATE_MXCSR_OFFSET;
    storage[mxcsr..mxcsr + 4].copy_from_slice(&0x1f80_u32.to_le_bytes());
    storage[8..16].copy_from_slice(&CODE_DESCRIPTOR.to_le_bytes());
    storage[16..24].copy_from_slice(&DATA_DESCRIPTOR.to_le_bytes());
    let tss = tss_descriptor(layout.base + TSS_OFFSET as u64);
    storage[24..32].copy_from_slice(&tss[0].to_le_bytes());
    storage[32..40].copy_from_slice(&tss[1].to_le_bytes());
    storage[TSS_OFFSET + 4..TSS_OFFSET + 12]
        .copy_from_slice(&layout.host_stack.top().to_le_bytes());
    for (index, top) in layout.ist_tops.iter().enumerate() {
        let offset = TSS_OFFSET + 36 + index * 8;
        storage[offset..offset + 8].copy_from_slice(&top.to_le_bytes());
    }
    // No I/O bitmap exists. Its base is beyond the TSS's inclusive 0x67 limit.
    storage[TSS_OFFSET + 102..TSS_OFFSET + 104].copy_from_slice(&(TSS_BYTES as u16).to_le_bytes());
    for vector in 0..IDT_ENTRIES {
        let offset = IDT_OFFSET + vector * IDT_GATE_BYTES;
        storage[offset..offset + IDT_GATE_BYTES]
            .copy_from_slice(&gates[usize::from(vector_ist(vector) - 1)]);
    }
}

/// Initialized private descriptor/exception state borrowing stable backing pages.
///
/// This is not `Copy` or `Clone`: copying addresses does not transfer ownership
/// of a live TSS or its stacks to another CPU. Keeping this value alive does not
/// itself install a VMCS or make its mappings correct; see [`Self::initialize`].
pub struct HostEnvironment<'storage> {
    layout: Layout,
    _storage: PhantomData<&'storage mut [u8]>,
}

impl<'storage> HostEnvironment<'storage> {
    /// Initializes all tables/stacks after validating the complete layout.
    ///
    /// No privileged instruction executes here. On `Err`, `storage` is unchanged.
    /// No guard pages are implied: a paging owner must explicitly create guards
    /// if desired, outside this reserved, fully mapped block.
    ///
    /// # Safety
    ///
    /// The caller exclusively owns `storage` and `host_stack`, and no CPU/VMCS
    /// currently references either allocation as live descriptor or stack state.
    /// Their linear addresses must remain stable and mapped supervisor-writable
    /// by HOST_CR3 until every referencing VMCS/CPU is retired. The exception
    /// handler and message must remain executable/readable in that same map;
    /// use [`Self::required_image_addresses`] when validating the resident image.
    /// Hardware must enter this environment at CPL0 in 64-bit mode, with CR4.CET
    /// clear and no FS/GS-based TLS. Each simultaneously running CPU needs its
    /// own storage and normal host stack. After any VM exit selects this state,
    /// returning to firmware requires a separate complete architectural restore;
    /// dropping this value does not perform one or authorize freeing its pages.
    pub unsafe fn initialize(
        storage: &'storage mut [u8],
        host_stack: HostStack,
        linear_bits: u8,
    ) -> Result<Self, Error> {
        let entry = exception_stop as *const () as usize as u64;
        let layout = Layout::new(
            storage.as_ptr() as usize as u64,
            storage.len(),
            host_stack,
            linear_bits,
            entry,
        )?;
        let mut gates = [[0; IDT_GATE_BYTES]; IST_COUNT];
        for (index, gate) in gates.iter_mut().enumerate() {
            *gate = idt_gate(entry, (index + 1) as u8, linear_bits)?;
        }
        let message = EXCEPTION_MESSAGE_BYTES.as_ptr() as usize as u64;
        let message_end = message
            .checked_add(EXCEPTION_MESSAGE_BYTES.len() as u64)
            .ok_or(Error::Overflow)?;
        check_range(message, message_end, linear_bits)?;
        let gp_entry = general_protection as *const () as usize as u64;
        if (layout.base..layout.end).contains(&gp_entry)
            || (host_stack.base..host_stack.end).contains(&gp_entry)
        {
            return Err(Error::HandlerOverlap);
        }
        let gp_gate = idt_gate(gp_entry, vector_ist(13), linear_bits)?;
        populate(storage, layout, gates);
        let gp_offset = IDT_OFFSET + 13 * IDT_GATE_BYTES;
        storage[gp_offset..gp_offset + IDT_GATE_BYTES].copy_from_slice(&gp_gate);
        // The caller publishes VMCS pointers only after the complete table image.
        compiler_fence(Ordering::Release);
        Ok(Self {
            layout,
            _storage: PhantomData,
        })
    }

    /// Returns the descriptor/IST range to reserve and exclude from normal L1 RAM.
    #[must_use]
    pub const fn storage_range(&self) -> (u64, u64) {
        (self.layout.base, self.layout.end)
    }

    /// Binds the owning monitor's CPU-local metadata before publishing a VMCS.
    /// The HAL does not interpret or borrow the pointed-to monitor structure.
    ///
    /// # Safety
    ///
    /// No CPU may currently use this environment. `data` must identify the
    /// owning CPU's initialized, suitably aligned monitor object, retained and
    /// mapped in HOST_CR3 for the entire environment lifetime. The monitor must
    /// enforce its object's aliasing and synchronization contract when reading
    /// the pointer; this binding does not grant concurrent mutable references.
    pub unsafe fn bind_monitor_data(&mut self, data: NonNull<()>) {
        let address = self.layout.base + (XSTATE_OFFSET + MONITOR_DATA) as u64;
        // SAFETY: initialize validated this word's bounds/alignment in our
        // exclusively borrowed storage. The caller excludes all hardware use
        // until publication; this writes the pointer, not the opaque object.
        unsafe { (address as *mut usize).write(data.as_ptr() as usize) };
        compiler_fence(Ordering::Release);
    }

    /// Installs the private descriptors and GS before an AP prepares VMX.
    /// Its existing reserved bootstrap stack remains current; the first VM exit
    /// will select the dedicated host stack supplied to `initialize`.
    ///
    /// # Safety
    ///
    /// This is a one-time transition on the owning CPU at CPL0 in 64-bit mode,
    /// with IF clear, CR4.CET clear, and no firmware/guest execution to return to.
    /// The TSS must still be available (never previously loaded with LTR).
    /// `bind_monitor_data` must already have published this CPU's live object.
    /// The private host map is active and covers all backing, the current stack
    /// and this code. No other CPU may use these tables. After this call, even a
    /// failed VMXON must retain the environment; firmware rollback is not valid.
    pub unsafe fn install_ap_bootstrap(&self) {
        #[repr(C, packed)]
        struct Pointer {
            limit: u16,
            base: u64,
        }
        let gdt = Pointer {
            limit: 39,
            base: self.layout.base,
        };
        let idt = Pointer {
            limit: (IDT_ENTRIES * IDT_GATE_BYTES - 1) as u16,
            base: self.layout.base + IDT_OFFSET as u64,
        };
        let gs = self.layout.base + XSTATE_OFFSET as u64;
        // SAFETY: initialize encoded a busy TSS for VMX host-state loading,
        // which does not execute LTR. This first manual AP installation needs
        // an available descriptor instead. Only this CPU owns the mapped fresh
        // GDT; clear its busy bit, and LTR below sets it again atomically before
        // any guest or VMCS can consume the private host state.
        unsafe {
            let access = (self.layout.base + u64::from(HOST_TSS_SELECTOR) + 5) as *mut u8;
            access.write_volatile(0x89);
        }
        // SAFETY: the caller provides exclusive, retained and mapped descriptor
        // state plus a valid bootstrap stack. The far return balances its two
        // pushes and reloads CS before loading the new IDT. Fresh LTR marks only
        // this CPU's own TSS busy. GS is bound before the #GP guard can run; all
        // temporary table pointers remain on the unchanged bootstrap stack.
        unsafe {
            core::arch::asm!(
                "lgdt [r8]",
                "push {code}", "lea rax, [rip + 2f]", "push rax", "retfq", "2:",
                "mov ax, {data}", "mov ss, ax", "mov ds, ax", "mov es, ax",
                "xor eax, eax", "mov fs, ax", "mov gs, ax",
                "mov ax, {tss}", "ltr ax",
                "mov ecx, 0xc0000100", "xor eax, eax", "xor edx, edx", "wrmsr",
                "mov ecx, 0xc0000101", "mov rax, r10", "mov rdx, r10", "shr rdx, 32", "wrmsr",
                "lidt [r9]",
                in("r8") &gdt, in("r9") &idt, in("r10") gs,
                code = const HOST_CODE_SELECTOR, data = const HOST_DATA_SELECTOR,
                tss = const HOST_TSS_SELECTOR,
                out("rax") _, out("rcx") _, out("rdx") _,
            );
        }
    }

    /// Returns four disjoint downward-growing IST ranges, in architectural order.
    #[must_use]
    pub fn exception_stack_ranges(&self) -> [(u64, u64); IST_COUNT] {
        self.layout
            .ist_tops
            .map(|top| (top - IST_STACK_BYTES as u64, top))
    }

    /// Returns fatal handler, first/last message byte and guarded #GP entry addresses.
    ///
    /// All must belong to the retained runtime image and its HOST_CR3 map.
    #[must_use]
    pub fn required_image_addresses(&self) -> [u64; 4] {
        let message = EXCEPTION_MESSAGE_BYTES.as_ptr() as usize as u64;
        [
            self.layout.exception_entry,
            message,
            message + EXCEPTION_MESSAGE_BYTES.len() as u64 - 1,
            general_protection as *const () as usize as u64,
        ]
    }

    /// Returns all selector, descriptor/base, SYSENTER and RSP host VMCS fields.
    ///
    /// The owner additionally supplies HOST_CR0/CR3/CR4, HOST_RIP, and the host
    /// MSR fields required by its controls. FS base is zero; GS addresses this
    /// CPU's 64-byte-aligned FXSAVE64 scratch followed by a private MXCSR value.
    /// The VM-exit stub must save before using FP/SIMD and restore before entry;
    /// HOST_CR0.EM/TS must be clear and HOST_CR4.OSFXSR set. SYSENTER_CS is
    /// disabled; its otherwise-unused target and stack still belong to L0.
    #[must_use]
    pub fn vmcs_fields(&self) -> [(u32, u64); 16] {
        [
            (vmcs::HOST_ES_SELECTOR, u64::from(HOST_DATA_SELECTOR)),
            (vmcs::HOST_CS_SELECTOR, u64::from(HOST_CODE_SELECTOR)),
            (vmcs::HOST_SS_SELECTOR, u64::from(HOST_DATA_SELECTOR)),
            (vmcs::HOST_DS_SELECTOR, u64::from(HOST_DATA_SELECTOR)),
            (vmcs::HOST_FS_SELECTOR, 0),
            (vmcs::HOST_GS_SELECTOR, 0),
            (vmcs::HOST_TR_SELECTOR, u64::from(HOST_TSS_SELECTOR)),
            (vmcs::HOST_FS_BASE, 0),
            (vmcs::HOST_GS_BASE, self.layout.base + XSTATE_OFFSET as u64),
            (vmcs::HOST_TR_BASE, self.layout.base + TSS_OFFSET as u64),
            (vmcs::HOST_GDTR_BASE, self.layout.base),
            (vmcs::HOST_IDTR_BASE, self.layout.base + IDT_OFFSET as u64),
            (vmcs::HOST_IA32_SYSENTER_CS, 0),
            (vmcs::HOST_IA32_SYSENTER_ESP, self.layout.host_stack.top()),
            (vmcs::HOST_IA32_SYSENTER_EIP, self.layout.exception_entry),
            (vmcs::HOST_RSP, self.layout.host_stack.vmexit_rsp()),
        ]
    }
}

/// Retrieves the explicitly bound, opaque CPU-local monitor object, if any.
///
/// # Safety
///
/// This CPU must run in this module's initialized private host environment,
/// with its own GS base installed and no CPU migration. The caller must uphold
/// the bound object's lifetime, type and aliasing contract before dereferencing
/// it. The pointer is not a lock guard or a mutable-reference capability.
pub unsafe fn monitor_data() -> Option<NonNull<()>> {
    let address: usize;
    // SAFETY: the caller establishes the private GS base and live scratch.
    // This reads one naturally aligned pointer word without mutating the object.
    unsafe {
        core::arch::asm!(
            "mov {address}, gs:[{offset}]",
            address = out(reg) address,
            offset = const MONITOR_DATA,
            options(readonly, nostack, preserves_flags),
        );
    }
    NonNull::new(address as *mut ())
}

/// Reads an MSR, recovering only this instruction's architectural #GP(0).
///
/// This does not validate VMX MSR-list format or model-specific list exclusions.
/// A successful ordinary RDMSR alone does not establish that hardware may use
/// that MSR in a VM-entry/exit list, or that it remains readable after L2 runs.
///
/// # Safety
///
/// The calling CPU must run at CPL0 in this initialized private host environment:
/// HOST_GS_BASE names its uniquely owned scratch, this module's IDT/TSS/IST and
/// code are live in HOST_CR3, IF is clear, and CR4.CET is clear. No nested probe
/// or CPU migration is allowed. The caller must allow any read side effects of
/// the selected MSR; this function neither writes MSRs nor changes CR2/XSTATE.
pub unsafe fn try_rdmsr(index: u32) -> Option<u64> {
    let low: u32;
    let high: u32;
    let faulted: u64;
    // SAFETY: the caller guarantees private per-CPU GS/IDT/IST ownership and
    // non-reentrancy. Only label 2's RDMSR is armed; the #GP gate checks its RIP,
    // ring-zero CS and zero error code, then resumes at label 3 without changing
    // GPRs. Every return disarms the probe before Rust observes the result.
    unsafe {
        core::arch::asm!(
            "cmp qword ptr gs:[{armed}], 0",
            "jne {fatal}",
            "lea rax, [rip + 3f]",
            "mov gs:[{resume}], rax",
            "mov qword ptr gs:[{failed}], 0",
            "lea rax, [rip + 2f]",
            "mov gs:[{armed}], rax",
            "2:",
            "rdmsr",
            "3:",
            "mov qword ptr gs:[{armed}], 0",
            "mov {faulted}, gs:[{failed}]",
            armed = const MSR_FAULT_RIP,
            resume = const MSR_RESUME_RIP,
            failed = const MSR_FAULTED,
            fatal = sym exception_stop,
            in("ecx") index,
            out("eax") low,
            out("edx") high,
            faulted = out(reg) faulted,
        );
    }
    (faulted == 0).then_some(u64::from(low) | (u64::from(high) << 32))
}

/// Writes an MSR, recovering only this instruction's architectural #GP(0).
///
/// # Safety
/// The private GS/IDT/IST, CPL0, IF=0, CET=0 and non-reentrancy requirements of
/// `try_rdmsr` apply. The caller must additionally permit all successful write
/// side effects, including changes to architectural mode or interrupt delivery.
/// This is not authorization to write an arbitrary guest-selected host MSR.
pub unsafe fn try_wrmsr(index: u32, value: u64) -> bool {
    let faulted: u64;
    // SAFETY: the caller establishes the private CPU scratch and permitted MSR
    // side effects. Only label 2's WRMSR is armed. The #GP gate validates exact
    // RIP/CS/error and preserves the saved GPRs; every return disarms the probe.
    unsafe {
        core::arch::asm!(
            "cmp qword ptr gs:[{armed}], 0", "jne {fatal}",
            "lea rax, [rip + 3f]", "mov gs:[{resume}], rax",
            "mov qword ptr gs:[{failed}], 0",
            "lea rax, [rip + 2f]", "mov gs:[{armed}], rax",
            "mov rax, {value}", "mov rdx, {value}", "shr rdx, 32",
            "2:", "wrmsr", "3:",
            "mov qword ptr gs:[{armed}], 0",
            "mov {faulted}, gs:[{failed}]",
            armed = const MSR_FAULT_RIP, resume = const MSR_RESUME_RIP,
            failed = const MSR_FAULTED, fatal = sym exception_stop,
            in("ecx") index, value = in(reg) value,
            out("rax") _, out("rdx") _, faulted = out(reg) faulted,
        );
    }
    faulted == 0
}

/// Recovers an exact armed MSR-access fault; every other root #GP remains fatal.
// SAFETY: a 64-bit CPL0 interrupt gate uses the CPU-owned IST4, with error code,
// RIP, CS, RFLAGS, old RSP and SS at offsets 0..40. GS remains private; this gate
// neither swaps GS nor calls Rust nor touches FP state. Two saved GPRs shift
// the error/RIP/CS slots to 16/24/32. IRETQ restores the original stack and flags.
#[unsafe(naked)]
extern "C" fn general_protection() -> ! {
    core::arch::naked_asm!(
        "push rax",
        "push rcx",
        "cmp qword ptr [rsp + 16], 0",
        "jne {fatal}",
        "cmp qword ptr [rsp + 32], {cs}",
        "jne {fatal}",
        "mov rax, gs:[{armed}]",
        "test rax, rax",
        "jz {fatal}",
        "cmp rax, [rsp + 24]",
        "jne {fatal}",
        "mov rcx, gs:[{resume}]",
        "test rcx, rcx",
        "jz {fatal}",
        "mov qword ptr gs:[{failed}], 1",
        "mov [rsp + 24], rcx",
        "pop rcx",
        "pop rax",
        "add rsp, 8",
        "iretq",
        cs = const HOST_CODE_SELECTOR,
        armed = const MSR_FAULT_RIP,
        resume = const MSR_RESUME_RIP,
        failed = const MSR_FAULTED,
        fatal = sym exception_stop,
    );
}

/// Fatal-only entry, deliberately independent of Rust stack frames and TLS.
// SAFETY: only a CPL0 64-bit IDT/disabled-SYSENTER target may enter this code.
// The caller maps this function/message and the selected IST stack permanently.
// It never resumes the interrupted context, so clobbering GPRs is intentional;
// each UART byte has a finite poll budget, and no firmware function is invoked.
#[unsafe(naked)]
extern "C" fn exception_stop() -> ! {
    core::arch::naked_asm!(
        "cli",
        "lea rsi, [rip + {message}]",
        "mov edi, {length}",
        "2:",
        "mov dx, 0x3fd",
        "mov ecx, 65536",
        "3:",
        "in al, dx",
        "cmp al, 0xff",
        "je 5f",
        "test al, 0x20",
        "jnz 4f",
        "pause",
        "dec ecx",
        "jnz 3b",
        "jmp 5f",
        "4:",
        "mov dx, 0x3f8",
        "mov al, byte ptr [rsi]",
        "out dx, al",
        "inc rsi",
        "dec edi",
        "jnz 2b",
        "5:",
        "hlt",
        "jmp 5b",
        message = sym EXCEPTION_MESSAGE_BYTES,
        length = const EXCEPTION_MESSAGE_BYTES.len(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C, align(4096))]
    struct AlignedStorage([u8; HOST_ENVIRONMENT_BYTES]);

    #[repr(C, align(4096))]
    struct AlignedStack([u8; 4 * HOST_PAGE_BYTES]);

    fn owned_stack(backing: &mut AlignedStack) -> HostStack {
        HostStack::new(
            backing.0.as_mut_ptr() as usize as u64,
            backing.0.len() as u64,
        )
        .unwrap()
    }

    fn stack() -> HostStack {
        HostStack::new(0x10_0000, 4 * HOST_PAGE_BYTES as u64).unwrap()
    }

    fn layout(base: u64, bits: u8) -> Result<Layout, Error> {
        Layout::new(base, HOST_ENVIRONMENT_BYTES, stack(), bits, 0x20_0000)
    }

    fn word(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes.try_into().unwrap())
    }

    #[test]
    fn private_descriptor_encodings_match_long_mode_and_busy_tss() {
        let mut storage = AlignedStorage([0xa5; HOST_ENVIRONMENT_BYTES]);
        let planned = layout(0x30_0000, 48).unwrap();
        let gates = core::array::from_fn(|index| idt_gate(0x20_0000, index as u8 + 1, 48).unwrap());
        populate(&mut storage.0, planned, gates);
        assert_eq!(word(&storage.0[..8]), 0);
        assert_eq!(word(&storage.0[8..16]), CODE_DESCRIPTOR);
        assert_eq!(word(&storage.0[16..24]), DATA_DESCRIPTOR);
        let tss = tss_descriptor(0x30_0040);
        assert_eq!(word(&storage.0[24..32]), tss[0]);
        assert_eq!(word(&storage.0[32..40]), tss[1]);
        assert_eq!((tss[0] >> 40) & 0xff, 0x8b);
        assert_eq!(tss[0] & 0xffff, 0x67);
        assert_eq!((CODE_DESCRIPTOR >> 53) & 3, 1); // L=1, D=0.
        assert_eq!(
            word(&storage.0[TSS_OFFSET + 4..TSS_OFFSET + 12]),
            stack().top()
        );
        assert_eq!(&storage.0[TSS_OFFSET + 102..TSS_OFFSET + 104], &[104, 0]);
        assert!(
            storage.0[TSS_OFFSET + 12..TSS_OFFSET + 36]
                .iter()
                .all(|&b| b == 0)
        );
        assert!(
            storage.0[TSS_OFFSET + 68..TSS_OFFSET + 102]
                .iter()
                .all(|&b| b == 0)
        );
    }

    #[test]
    fn all_idt_gates_use_valid_targets_and_dedicated_ist_stacks() {
        let mut storage = AlignedStorage([0; HOST_ENVIRONMENT_BYTES]);
        let planned = layout(0x30_0000, 48).unwrap();
        let entry = 0xffff_8000_1234_5678;
        let gates = core::array::from_fn(|index| idt_gate(entry, index as u8 + 1, 48).unwrap());
        populate(&mut storage.0, planned, gates);
        for vector in 0..IDT_ENTRIES {
            let offset = IDT_OFFSET + vector * IDT_GATE_BYTES;
            let gate = &storage.0[offset..offset + IDT_GATE_BYTES];
            assert_eq!(gate[4], vector_ist(vector));
            assert_eq!(gate[5], 0x8e);
            assert_eq!(&gate[2..4], &HOST_CODE_SELECTOR.to_le_bytes());
            assert_eq!(&gate[12..16], &[0; 4]);
            let decoded = u64::from(u16::from_le_bytes(gate[..2].try_into().unwrap()))
                | (u64::from(u16::from_le_bytes(gate[6..8].try_into().unwrap())) << 16)
                | (u64::from(u32::from_le_bytes(gate[8..12].try_into().unwrap())) << 32);
            assert_eq!(decoded, entry);
        }
        assert_eq!(
            (vector_ist(8), vector_ist(2), vector_ist(18), vector_ist(14)),
            (1, 2, 3, 4)
        );
        for (index, top) in planned.ist_tops.into_iter().enumerate() {
            let offset = TSS_OFFSET + 36 + index * 8;
            assert_eq!(word(&storage.0[offset..offset + 8]), top);
            assert_eq!(top & 15, 0);
            assert!(top - IST_STACK_BYTES as u64 >= planned.base + IST_STACKS_OFFSET as u64);
            assert!(top <= planned.end);
            if index != 0 {
                assert_eq!(planned.ist_tops[index - 1], top - IST_STACK_BYTES as u64);
            }
        }
        assert!(storage.0[IST_STACKS_OFFSET..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn layout_rejects_overflow_noncanonical_ranges_and_stack_overlap() {
        assert_eq!(HostStack::new(0, 4096), Err(Error::EmptyRange));
        assert_eq!(HostStack::new(4096, 0), Err(Error::EmptyRange));
        assert_eq!(HostStack::new(4097, 4096), Err(Error::Alignment));
        assert_eq!(HostStack::new(4096, 4097), Err(Error::Alignment));
        assert_eq!(HostStack::new(u64::MAX - 4095, 4096), Err(Error::Overflow));
        assert_eq!(layout(0x30_0000, 47), Err(Error::LinearAddressWidth));
        assert_eq!(layout(0x30_0001, 48), Err(Error::Alignment));
        assert_eq!(layout(u64::MAX - 4095, 48), Err(Error::Overflow));
        assert_eq!(layout(1 << 47, 48), Err(Error::Noncanonical));
        assert_eq!(
            layout((1 << 47) - HOST_ENVIRONMENT_BYTES as u64, 48),
            Err(Error::Noncanonical)
        );
        assert_eq!(layout(0x10_0000, 48), Err(Error::Overlap));
        assert_eq!(layout(0x20_0000, 48), Err(Error::HandlerOverlap));
        let noncanonical_stack = HostStack::new(1 << 47, HOST_PAGE_BYTES as u64).unwrap();
        assert_eq!(
            Layout::new(
                0x30_0000,
                HOST_ENVIRONMENT_BYTES,
                noncanonical_stack,
                48,
                0x20_0000
            ),
            Err(Error::Noncanonical)
        );
        assert!(layout(0xffff_8000_0030_0000, 48).is_ok());
        assert!(layout(0x0001_0000_0030_0000, 57).is_ok());
        assert_eq!(layout(0x0001_0000_0030_0000, 48), Err(Error::Noncanonical));
        assert_eq!(
            check_range(0x1000, 0xffff_8000_0000_1000, 48),
            Err(Error::Noncanonical)
        );
        assert_eq!(idt_gate(0x1000, 0, 48), Err(Error::IstIndex));
        assert_eq!(idt_gate(0x1000, 8, 48), Err(Error::IstIndex));
        assert_eq!(idt_gate(1 << 47, 1, 48), Err(Error::Noncanonical));
    }

    #[test]
    fn failed_initialization_keeps_reserved_bytes_unchanged() {
        let mut storage = AlignedStorage([0xa5; HOST_ENVIRONMENT_BYTES]);
        let mut host_stack_bytes = AlignedStack([0; 4 * HOST_PAGE_BYTES]);
        let host_stack = owned_stack(&mut host_stack_bytes);
        // SAFETY: this test owns both inert buffers; no hardware references them.
        let result = unsafe { HostEnvironment::initialize(&mut storage.0[..4096], host_stack, 48) };
        assert!(matches!(result, Err(Error::StorageSize)));
        assert!(storage.0.iter().all(|&byte| byte == 0xa5));
        // SAFETY: both buffers are test-owned and inert; invalid width fails before writes.
        let result = unsafe { HostEnvironment::initialize(&mut storage.0, host_stack, 64) };
        assert!(matches!(result, Err(Error::LinearAddressWidth)));
        assert!(storage.0.iter().all(|&byte| byte == 0xa5));
    }

    #[test]
    fn vmcs_host_fields_are_complete_private_and_distinct_per_instance() {
        let mut first_storage = AlignedStorage([0; HOST_ENVIRONMENT_BYTES]);
        let mut second_storage = AlignedStorage([0; HOST_ENVIRONMENT_BYTES]);
        let mut first_stack_bytes = AlignedStack([0; 4 * HOST_PAGE_BYTES]);
        let mut second_stack_bytes = AlignedStack([0; 4 * HOST_PAGE_BYTES]);
        let first_stack = owned_stack(&mut first_stack_bytes);
        let second_stack = owned_stack(&mut second_stack_bytes);
        // SAFETY: this table/stack pair is test-owned and no VMCS/CPU references it.
        let mut first =
            unsafe { HostEnvironment::initialize(&mut first_storage.0, first_stack, 48) }.unwrap();
        // SAFETY: this distinct test-owned table/stack pair is likewise hardware-inert.
        let mut second =
            unsafe { HostEnvironment::initialize(&mut second_storage.0, second_stack, 48) }
                .unwrap();
        assert_ne!(first.storage_range(), second.storage_range());
        assert_ne!(
            first.exception_stack_ranges(),
            second.exception_stack_ranges()
        );
        assert_ne!(first_stack.range(), second_stack.range());
        let fields = first.vmcs_fields();
        for (index, (field, value)) in fields.iter().enumerate() {
            assert!(!fields[..index].iter().any(|(earlier, _)| earlier == field));
            if matches!(
                *field,
                vmcs::HOST_ES_SELECTOR
                    | vmcs::HOST_CS_SELECTOR
                    | vmcs::HOST_SS_SELECTOR
                    | vmcs::HOST_DS_SELECTOR
                    | vmcs::HOST_FS_SELECTOR
                    | vmcs::HOST_GS_SELECTOR
                    | vmcs::HOST_TR_SELECTOR
            ) {
                assert_eq!(value & 7, 0);
            }
        }
        let get = |field| {
            fields
                .iter()
                .find(|(actual, _)| *actual == field)
                .unwrap()
                .1
        };
        assert_eq!(get(vmcs::HOST_CS_SELECTOR), u64::from(HOST_CODE_SELECTOR));
        assert_eq!(get(vmcs::HOST_TR_SELECTOR), u64::from(HOST_TSS_SELECTOR));
        assert_eq!(get(vmcs::HOST_GDTR_BASE), first.storage_range().0);
        assert_eq!(
            get(vmcs::HOST_TR_BASE),
            first.storage_range().0 + TSS_OFFSET as u64
        );
        assert_eq!(
            get(vmcs::HOST_IDTR_BASE),
            first.storage_range().0 + IDT_OFFSET as u64
        );
        assert_eq!(get(vmcs::HOST_RSP), first_stack.vmexit_rsp());
        assert_eq!(get(vmcs::HOST_RSP) & 15, 8);
        assert_eq!(get(vmcs::HOST_FS_BASE), 0);
        let scratch = get(vmcs::HOST_GS_BASE);
        assert_eq!(scratch, first.storage_range().0 + XSTATE_OFFSET as u64);
        assert_eq!(scratch & 63, 0);
        assert_ne!(
            scratch,
            second
                .vmcs_fields()
                .into_iter()
                .find(|(field, _)| *field == vmcs::HOST_GS_BASE)
                .unwrap()
                .1
        );
        assert_eq!(get(vmcs::HOST_IA32_SYSENTER_CS), 0);
        assert_eq!(
            get(vmcs::HOST_IA32_SYSENTER_EIP),
            first.required_image_addresses()[0]
        );
        assert!(
            first
                .required_image_addresses()
                .into_iter()
                .all(|address| canonical(address, 48))
        );
        let mut first_data = 1_u64;
        let mut second_data = 2_u64;
        let first_pointer = NonNull::from(&mut first_data).cast();
        let second_pointer = NonNull::from(&mut second_data).cast();
        // SAFETY: both initialized objects and environments are test-owned,
        // distinct and stable until their final use here; no CPU loads the tables.
        unsafe {
            first.bind_monitor_data(first_pointer);
            second.bind_monitor_data(second_pointer);
        }
        // The borrow is no longer used; no hardware observes these test pages.
        assert_eq!(
            word(&first_storage.0[XSTATE_OFFSET + MONITOR_DATA..][..8]),
            first_pointer.as_ptr() as usize as u64
        );
        assert_eq!(
            word(&second_storage.0[XSTATE_OFFSET + MONITOR_DATA..][..8]),
            second_pointer.as_ptr() as usize as u64
        );
        assert!(
            first_storage.0[XSTATE_OFFSET..XSTATE_OFFSET + 512]
                .iter()
                .all(|byte| *byte == 0)
        );
        let mxcsr = XSTATE_OFFSET + HOST_XSTATE_MXCSR_OFFSET;
        assert_eq!(
            &first_storage.0[mxcsr..mxcsr + 4],
            &0x1f80_u32.to_le_bytes()
        );
        for offset in [MSR_FAULT_RIP, MSR_RESUME_RIP, MSR_FAULTED] {
            assert_eq!(offset & 7, 0);
            assert!(offset >= HOST_XSTATE_MXCSR_OFFSET + 4);
            assert_eq!(word(&first_storage.0[XSTATE_OFFSET + offset..][..8]), 0);
        }
        let offset = IDT_OFFSET + 13 * IDT_GATE_BYTES;
        assert_eq!(
            &first_storage.0[offset..offset + IDT_GATE_BYTES],
            &idt_gate(general_protection as *const () as usize as u64, 4, 48).unwrap()
        );
        let offset = IDT_OFFSET + 14 * IDT_GATE_BYTES;
        assert_eq!(
            &first_storage.0[offset..offset + IDT_GATE_BYTES],
            &idt_gate(exception_stop as *const () as usize as u64, 4, 48).unwrap()
        );
    }
}
