#![allow(dead_code)]

extern crate alloc;

use aarch64_test::exit_failure;
use aarch64_test::exit_success;
use core::arch::asm;
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;
use cpu;
use exceptions;
use exceptions::registers::InstructionRegisterSize;
use exceptions::registers::SyndromeAccessSize;
use exceptions::registers::TI;
use exceptions::registers::WriteNotRead;
use exceptions::synchronous_handler::DataAbortInfo;
use exceptions::synchronous_handler::TrappedWfInfo;
use gic::BinaryPoint;
use gic::EnableOp;
use gic::EoiMode;
use gic::GicCpuConfig;
use gic::GicCpuInterface;
use gic::GicDistributor;
use gic::GicError;
use gic::GicIrqMirror;
use gic::GicPpi;
use gic::GicSgi;
use gic::IrqGroup;
use gic::IrqSense;
use gic::MmioRegion;
use gic::PIntId;
use gic::SgiTarget;
use gic::SpiRoute;
use gic::TriggerMode;
use gic::VIntId;
use gic::VcpuId;
use gic::VgicHw;
use gic::gicv2::GICV2_GICC_FRAME_SIZE;
use gic::gicv2::GICV2_GICD_FRAME_SIZE;
use gic::gicv2::Gicv2;
use gic::gicv2::Gicv2AccessSize;
use gic::gicv2::Gicv2DistIdRegs;
use gic::gicv2::Gicv2VirtualizationRegion;
use gic::vm::manager::VgicDelegate;
use gic::vm::manager::VgicManager;
use paging::stage2::Stage2AccessPermission;
use paging::stage2::Stage2PageTypes;
use paging::stage2::Stage2Paging;
use paging::stage2::Stage2PagingSetting;
use print::debug_uart;
use print::println;
use tls;

pub const UART_BASE: usize = 0x0900_0000;
pub const UART_SIZE: usize = 0x1000;
pub const UART_CLOCK_HZ: u32 = 48 * 1_000_000;

pub const GICD_BASE: usize = 0x0800_0000;
pub const GICC_BASE: usize = 0x0801_0000;
pub const GICH_BASE: usize = 0x0803_0000;
pub const GICV_BASE: usize = 0x0804_0000;

pub const GICH_SIZE: usize = 0x1000;
pub const GICV_SIZE: usize = 0x2000;

pub const SGI_ID: u32 = 1;
pub const MAINT_INTID: u32 = 25;
pub const TIMER_TEST_PPI_INTID: u32 = 27;
pub const UART_SPI_INTID: u32 = 33;
pub const ALT_SPURIOUS_INTID: u32 = 1022;
pub const SPURIOUS_INTID: u32 = 1023;

pub const KICK_SGI_ID: u8 = 15;
pub const KICK_INTID: u32 = KICK_SGI_ID as u32;

pub const INT_KIND_COUNT: usize = 3;
pub const IDX_SGI: usize = 0;
pub const IDX_PPI: usize = 1;
pub const IDX_UART: usize = 2;

pub const POLL_TIMEOUT_ITERS: usize = 200_000;
pub const DUPLICATE_CHECK_ITERS: usize = 8_192;
pub const IRQ_WAIT_ITERS: usize = 200_000;

pub const TEST_CTRL_BASE: usize = 0x0810_0000;
pub const TEST_CTRL_SIZE: usize = 0x1000;
pub const TEST_CTRL_CMD_OFF: usize = 0x000;

pub const FAIL_EL2_STAGE2_INIT: u32 = 0x1001;
pub const FAIL_EL2_GIC_INIT: u32 = 0x1002;
pub const FAIL_EL2_VGIC_INIT: u32 = 0x1003;
pub const FAIL_EL2_DABORT_NO_ACCESS: u32 = 0x1101;
pub const FAIL_EL2_DABORT_BAD_WIDTH: u32 = 0x1102;
pub const FAIL_EL2_DABORT_BAD_REGISTER: u32 = 0x1103;
pub const FAIL_EL2_DABORT_BAD_ADDR: u32 = 0x1104;
pub const FAIL_EL2_DABORT_READ: u32 = 0x1105;
pub const FAIL_EL2_DABORT_WRITE: u32 = 0x1106;
pub const FAIL_EL2_TEST_CTRL_BAD_ACCESS: u32 = 0x1107;
pub const FAIL_EL2_TEST_CTRL_UNSUPPORTED_CMD: u32 = 0x1108;
pub const FAIL_EL2_IRQ_UNSUPPORTED: u32 = 0x1201;
pub const FAIL_EL2_IRQ_HANDLE: u32 = 0x1202;
pub const FAIL_EL2_IRQ_EOI: u32 = 0x1203;
pub const FAIL_EL2_IRQ_DEACT: u32 = 0x1204;

pub const FAIL_POLL_SGI_TIMEOUT: u32 = 0x2001;
pub const FAIL_POLL_SGI_UNEXPECTED: u32 = 0x2002;
pub const FAIL_POLL_SGI_DUPLICATE: u32 = 0x2003;
pub const FAIL_POLL_SGI_DISABLED_DELIVERED: u32 = 0x2004;
pub const FAIL_POLL_SGI_REENABLE_TIMEOUT: u32 = 0x2005;
pub const FAIL_POLL_SGI_DISABLE_AFTER_DELIVER_DUPLICATE: u32 = 0x2006;
pub const FAIL_POLL_PPI_TIMEOUT: u32 = 0x2011;
pub const FAIL_POLL_PPI_UNEXPECTED: u32 = 0x2012;
pub const FAIL_POLL_PPI_DUPLICATE: u32 = 0x2013;
pub const FAIL_POLL_UART_TIMEOUT: u32 = 0x2021;
pub const FAIL_POLL_UART_UNEXPECTED: u32 = 0x2022;
pub const FAIL_POLL_UART_DUPLICATE: u32 = 0x2023;

pub const FAIL_IRQ_SGI_WAIT_TIMEOUT: u32 = 0x3001;
pub const FAIL_IRQ_SGI_SECOND_WAIT_TIMEOUT: u32 = 0x3002;
pub const FAIL_IRQ_PPI_WAIT_TIMEOUT: u32 = 0x3011;
pub const FAIL_IRQ_UART_WAIT_TIMEOUT: u32 = 0x3021;
pub const FAIL_IRQ_UNEXPECTED_INTID: u32 = 0x3031;

pub const FAIL_PHYS_PPI_WAIT_TIMEOUT: u32 = 0x4001;
pub const FAIL_PHYS_PPI_SECOND_WAIT_TIMEOUT: u32 = 0x4002;
pub const FAIL_PHYS_PPI_DELIVERED_AFTER_DISABLE: u32 = 0x4003;
pub const FAIL_PHYS_PPI_UNEXPECTED_INTID: u32 = 0x4031;

pub const FAIL_RPI_BOOT_PPI_WAIT_TIMEOUT: u32 = 0x4101;
pub const FAIL_RPI_BOOT_PPI_SECOND_WAIT_TIMEOUT: u32 = 0x4102;
pub const FAIL_RPI_BOOT_PPI_DELIVERED_AFTER_DISABLE: u32 = 0x4103;
pub const FAIL_RPI_BOOT_PPI_UNEXPECTED_INTID: u32 = 0x4131;

const GICD_CTLR_OFF: usize = 0x000;
const GICD_IGROUPR0_OFF: usize = 0x080;
const GICD_ISENABLER0_OFF: usize = 0x100;
const GICD_ICENABLER0_OFF: usize = 0x180;
const GICD_ICPENDR0_OFF: usize = 0x280;
const GICD_ISPENDR0_OFF: usize = 0x200;
const GICD_IPRIORITYR_OFF: usize = 0x400;
const GICD_ITARGETSR_OFF: usize = 0x800;
const GICD_SGIR_OFF: usize = 0xF00;

const GICV_CTLR_OFF: usize = 0x000;
const GICV_PMR_OFF: usize = 0x004;
const GICV_BPR_OFF: usize = 0x008;
const GICV_IAR_OFF: usize = 0x00C;
const GICV_EOIR_OFF: usize = 0x010;

const UART_DR_OFF: usize = 0x000;
const UART_CR_OFF: usize = 0x030;
const UART_IMSC_OFF: usize = 0x038;
const UART_ICR_OFF: usize = 0x044;

const UART_CR_UARTEN: u32 = 1 << 0;
const UART_CR_TXE: u32 = 1 << 8;
const UART_CR_RXE: u32 = 1 << 9;
const UART_IMSC_TXIM: u32 = 1 << 5;
const UART_ICR_ALL: u32 = 0x7ff;

const TEST_CTRL_CMD_PPI27_STICKY_ENABLE: u32 = 0x0000_0001;
const TEST_CTRL_CMD_PPI27_STICKY_DISABLE: u32 = 0x0000_0002;

const EL1_STACK_SIZE: usize = 0x4000;
const TLS_BUF_SIZE: usize = 4096;
const TEST_HEAP_SIZE: usize = 8 * 1024 * 1024;

const SPSR_EL2_M_EL1H: u64 = 0b0101;
const SPSR_EL2_DAIF_MASKED: u64 = 0b1111 << 6;

unsafe extern "C" {
    static __bss_start: u8;
    static __bss_end: u8;
}

#[repr(C)]
pub struct Shared {
    pub done: u32,
    pub fail: u32,
    pub poll_seen: [u32; INT_KIND_COUNT],
    pub irq_seen: [u32; INT_KIND_COUNT],
    pub last_intid: u32,
    pub unexpected_intid: u32,
    pub last_iar_raw: u32,
    pub reserved: u32,
}

impl Shared {
    pub const fn new() -> Self {
        Self {
            done: 0,
            fail: 0,
            poll_seen: [0; INT_KIND_COUNT],
            irq_seen: [0; INT_KIND_COUNT],
            last_intid: SPURIOUS_INTID,
            unexpected_intid: SPURIOUS_INTID,
            last_iar_raw: 0,
            reserved: 0,
        }
    }
}

#[repr(align(16))]
struct El1Stack([u8; EL1_STACK_SIZE]);

#[repr(align(64))]
struct TlsBuf([u8; TLS_BUF_SIZE]);

struct TestVgicDelegate;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TestPpi27Mode {
    DirectVgicSticky,
    RpiBootLikeSticky,
    RpiBootLikeHwLrDropOnlyNoDeactivate,
}

static DELEGATE: TestVgicDelegate = TestVgicDelegate;
static VGIC: VgicManager<1> = VgicManager::new(&DELEGATE, 0);

static HEAP_READY: AtomicBool = AtomicBool::new(false);
static ALLOCATOR: allocator::DefaultAllocator = allocator::DefaultAllocator::new();

static mut SHARED: Shared = Shared::new();
static mut TEST_NAME: &'static str = "vgic_e2e";
static mut GIC: Option<Gicv2> = None;
static mut DIST_ID: Option<Gicv2DistIdRegs> = None;
static mut TLS_BUF: TlsBuf = TlsBuf([0; TLS_BUF_SIZE]);
static mut TEST_HEAP: [u8; TEST_HEAP_SIZE] = [0; TEST_HEAP_SIZE];
static mut EL1_STACK: El1Stack = El1Stack([0; EL1_STACK_SIZE]);
static mut UART_PHYS_INJECTED: bool = false;
static mut TEST_PPI27_STICKY_ACTIVE: bool = false;
static mut TEST_PPI27_MODE: TestPpi27Mode = TestPpi27Mode::DirectVgicSticky;

impl VgicDelegate for TestVgicDelegate {
    fn irq_mirror(&self) -> Result<&'static dyn GicIrqMirror, GicError> {
        Ok(gic_ref() as &'static dyn GicIrqMirror)
    }

    fn get_resident_affinity(
        &self,
        _vm_id: usize,
        _vcpu_id: u16,
    ) -> Result<Option<cpu::CoreAffinity>, GicError> {
        Ok(Some(cpu::get_current_core_id()))
    }

    fn get_home_affinity(
        &self,
        _vm_id: usize,
        _vcpu_id: u16,
    ) -> Result<cpu::CoreAffinity, GicError> {
        Ok(cpu::get_current_core_id())
    }

    fn get_current_vcpu(&self, _vm_id: usize) -> Result<VcpuId, GicError> {
        Ok(VcpuId(0))
    }

    fn kick_pcpu(&self, target: cpu::CoreAffinity) -> Result<(), GicError> {
        let targets = [target];
        gic_ref().send_sgi(KICK_SGI_ID, SgiTarget::Specific(&targets))
    }
}

pub unsafe fn clear_bss() {
    // SAFETY: linker symbols bound the .bss range for this test image.
    let (start, end) = unsafe {
        (
            &__bss_start as *const u8 as usize,
            &__bss_end as *const u8 as usize,
        )
    };
    if end > start {
        // SAFETY: [start, end) is valid writable .bss memory.
        unsafe {
            ptr::write_bytes(start as *mut u8, 0, end - start);
        }
    }
}

pub fn panic_exit(test_name: &str, info: &core::panic::PanicInfo<'_>) -> ! {
    println!("{}: PANIC: {}", test_name, info);
    exit_failure();
}

pub fn run_el2(test_name: &'static str, guest_entry: extern "C" fn(*mut Shared) -> !) -> ! {
    run_el2_with_ppi27_mode(test_name, guest_entry, TestPpi27Mode::DirectVgicSticky)
}

pub fn run_el2_rpi_boot_like(
    test_name: &'static str,
    guest_entry: extern "C" fn(*mut Shared) -> !,
) -> ! {
    run_el2_with_ppi27_mode(test_name, guest_entry, TestPpi27Mode::RpiBootLikeSticky)
}

pub fn run_el2_rpi_boot_like_hwlr_drop_only_no_deactivate(
    test_name: &'static str,
    guest_entry: extern "C" fn(*mut Shared) -> !,
) -> ! {
    run_el2_with_ppi27_mode(
        test_name,
        guest_entry,
        TestPpi27Mode::RpiBootLikeHwLrDropOnlyNoDeactivate,
    )
}

fn run_el2_with_ppi27_mode(
    test_name: &'static str,
    guest_entry: extern "C" fn(*mut Shared) -> !,
    ppi27_mode: TestPpi27Mode,
) -> ! {
    // SAFETY: masking interrupts is required before EL2 bring-up sequencing.
    unsafe {
        asm!("msr daifset, #0b1111", options(nostack, preserves_flags));
    }

    debug_uart::init(UART_BASE, UART_CLOCK_HZ as u64, 115200);

    // SAFETY: single-core test init publishes the selected harness mode and test label before
    // any EL2 handler can observe them.
    unsafe {
        *ptr::addr_of_mut!(TEST_PPI27_MODE) = ppi27_mode;
        *ptr::addr_of_mut!(TEST_NAME) = test_name;
    }

    reset_shared();

    if let Err(err) = init_el2_tls() {
        println!("{}: FAIL tls init: {}", test_name, err);
        exit_failure();
    }

    exceptions::setup_exception();
    install_el2_handlers();

    if let Err(err) = setup_allocator() {
        println!("{}: FAIL allocator init: {}", test_name, err);
        exit_failure();
    }

    if let Err(err) = setup_stage2() {
        record_fail(FAIL_EL2_STAGE2_INIT);
        println!("{}: FAIL stage2 init: {}", test_name, err);
        exit_failure();
    }

    if let Err(err) = init_gic_and_vgic() {
        record_fail(FAIL_EL2_GIC_INIT);
        println!("{}: FAIL gic/vgic init: {}", test_name, err);
        exit_failure();
    }

    // SAFETY: EL2 must accept physical interrupts so it can inject vIRQs.
    unsafe {
        asm!("msr daifclr, #0b1111", options(nostack, preserves_flags));
    }

    enter_el1(guest_entry, shared_ptr())
}

fn install_el2_handlers() {
    exceptions::synchronous_handler::set_data_abort_handler(el2_data_abort_handler);
    exceptions::irq_handler::set_irq_handler(el2_irq_handler);
    exceptions::synchronous_handler::set_trapped_wf_handler(el2_trapped_wf_handler);
}

fn reset_shared() {
    let shared = shared_mut_static();
    *shared = Shared::new();
    // SAFETY: single-core test flow resets synthetic physical source latches before guest entry.
    unsafe {
        *ptr::addr_of_mut!(UART_PHYS_INJECTED) = false;
        *ptr::addr_of_mut!(TEST_PPI27_STICKY_ACTIVE) = false;
    }
}

fn init_el2_tls() -> Result<(), &'static str> {
    let need = tls::template_size();
    if TLS_BUF_SIZE < need {
        return Err("tls buffer too small");
    }

    // SAFETY: TLS buffer is 64-byte aligned, unique in this single-core test, and lives forever.
    let tls_result = unsafe {
        let tls_ptr = ptr::addr_of_mut!(TLS_BUF.0).cast::<u8>();
        tls::init_current_cpu(NonNull::new_unchecked(tls_ptr), TLS_BUF_SIZE)
    };
    tls_result.map_err(|_| "tls init failed")
}

fn setup_allocator() -> Result<(), &'static str> {
    if HEAP_READY.load(Ordering::SeqCst) {
        return Ok(());
    }

    ALLOCATOR.init();
    // SAFETY: TEST_HEAP is dedicated test heap storage and remains valid for program lifetime.
    let heap_start = ptr::addr_of_mut!(TEST_HEAP) as *mut u8 as usize;
    ALLOCATOR.add_available_region(heap_start, TEST_HEAP_SIZE)?;
    ALLOCATOR.finalize()?;
    HEAP_READY.store(true, Ordering::SeqCst);
    Ok(())
}

fn setup_stage2() -> Result<(), &'static str> {
    let stage2 = [
        Stage2PagingSetting {
            ipa: GICV_BASE,
            pa: GICV_BASE,
            size: GICV_SIZE,
            types: Stage2PageTypes::Device,
            perm: Stage2AccessPermission::ReadWrite,
        },
        Stage2PagingSetting {
            ipa: UART_BASE,
            pa: UART_BASE,
            size: UART_SIZE,
            types: Stage2PageTypes::Device,
            perm: Stage2AccessPermission::ReadWrite,
        },
        Stage2PagingSetting {
            ipa: 0x4000_0000,
            pa: 0x4000_0000,
            size: 0x4000_0000,
            types: Stage2PageTypes::Normal,
            perm: Stage2AccessPermission::ReadWrite,
        },
    ];

    Stage2Paging::init_stage2paging(&stage2, &ALLOCATOR)
        .map_err(|_| "stage2 table setup failed")?;
    Stage2Paging::enable_stage2_translation(true, true);
    cpu::isb();
    Ok(())
}

fn init_gic_and_vgic() -> Result<(), &'static str> {
    let gic = Gicv2::new(
        MmioRegion {
            base: GICD_BASE,
            size: GICV2_GICD_FRAME_SIZE,
        },
        MmioRegion {
            base: GICC_BASE,
            size: GICV2_GICC_FRAME_SIZE,
        },
        Some(Gicv2VirtualizationRegion {
            gich: MmioRegion {
                base: GICH_BASE,
                size: GICH_SIZE,
            },
            gicv: MmioRegion {
                base: GICV_BASE,
                size: GICV_SIZE,
            },
            maintenance_interrupt_id: MAINT_INTID,
        }),
        None,
    )
    .map_err(|_| "gic new failed")?;

    gic.init_distributor()
        .map_err(|_| "gic distributor init failed")?;
    gic.init_cpu_interface()
        .map_err(|_| "gic cpu init failed")?;

    let eoi_mode = match current_test_ppi27_mode() {
        TestPpi27Mode::DirectVgicSticky | TestPpi27Mode::RpiBootLikeSticky => {
            EoiMode::DropAndDeactivate
        }
        TestPpi27Mode::RpiBootLikeHwLrDropOnlyNoDeactivate => EoiMode::DropOnly,
    };
    let cfg = GicCpuConfig {
        priority_mask: 0xff,
        enable_group0: true,
        enable_group1: true,
        binary_point: BinaryPoint::Common(1),
        eoi_mode,
    };
    GicCpuInterface::configure(&gic, &cfg).map_err(|_| "gic cpu configure failed")?;

    gic.set_ppi_enable(MAINT_INTID, true)
        .map_err(|_| "enable maintenance ppi failed")?;

    gic.hw_init().map_err(|_| "gic hw init failed")?;

    // SAFETY: single-core init moves the GIC value into static storage exactly once.
    unsafe {
        *ptr::addr_of_mut!(GIC) = Some(gic);
    }

    let gic = gic_ref();

    // SAFETY: one-time single-core vGIC creation before guest entry.
    unsafe {
        VGIC.init_from_gicv2(gic, 1)
            .map_err(|_| "vgic init_from_gicv2 failed")?;
    }

    VGIC.switch_in(gic, VcpuId(0), cpu::get_current_core_id())
        .map_err(|_| "vgic switch_in failed")?;

    VGIC.bind_local_pirq_passthrough(
        gic,
        PIntId(TIMER_TEST_PPI_INTID),
        VcpuId(0),
        VIntId(TIMER_TEST_PPI_INTID),
    )
    .map_err(|_| "vgic bind local ppi failed")?;

    VGIC.map_pirq(
        gic,
        PIntId(UART_SPI_INTID),
        VcpuId(0),
        VIntId(UART_SPI_INTID),
        IrqSense::Level,
        IrqGroup::Group1,
        0x80,
    )
    .map_err(|_| "vgic map_pirq failed")?;

    gic.configure_spi(
        UART_SPI_INTID,
        IrqGroup::Group1,
        0x80,
        TriggerMode::Level,
        SpiRoute::Specific(cpu::get_current_core_id()),
        EnableOp::Enable,
    )
    .map_err(|_| "configure physical uart spi failed")?;

    // SAFETY: distributor ID registers are immutable snapshot values recorded once.
    unsafe {
        *ptr::addr_of_mut!(DIST_ID) = Some(Gicv2DistIdRegs::from_hw_gicd(gic.distributor()));
    }

    Ok(())
}

fn enter_el1(guest_entry: extern "C" fn(*mut Shared) -> !, shared: *mut Shared) -> ! {
    // SAFETY: EL1 stack is dedicated memory mapped in guest Stage-2 RAM window.
    let sp_el1 = unsafe { ptr::addr_of!(EL1_STACK.0) as usize + EL1_STACK_SIZE } as u64;

    cpu::set_sp_el1(sp_el1);
    cpu::set_elr_el2(guest_entry as usize as u64);
    cpu::set_spsr_el2(SPSR_EL2_M_EL1H | SPSR_EL2_DAIF_MASKED);
    cpu::isb();

    // SAFETY: x0 carries the Shared pointer argument to EL1 entry, then eret transfers control.
    unsafe {
        asm!("eret", in("x0") shared as u64, options(noreturn));
    }
}

fn gic_ref() -> &'static Gicv2 {
    // SAFETY: GIC is initialized once before use and then treated as immutable.
    unsafe {
        (&*ptr::addr_of!(GIC))
            .as_ref()
            .expect("EL2 GIC is not initialized")
    }
}

fn dist_id() -> Gicv2DistIdRegs {
    // SAFETY: DIST_ID is initialized during EL2 setup and then read-only.
    unsafe { (*ptr::addr_of!(DIST_ID)).expect("vGIC distributor ID registers are not initialized") }
}

fn shared_static() -> &'static Shared {
    // SAFETY: shared state lives for the entire test image lifetime.
    unsafe { &*ptr::addr_of!(SHARED) }
}

fn shared_mut_static() -> &'static mut Shared {
    // SAFETY: test is single-core and mutates this state in a controlled EL2 flow.
    unsafe { &mut *ptr::addr_of_mut!(SHARED) }
}

fn current_test_name() -> &'static str {
    // SAFETY: test name is set once at EL2 startup before any handler can read it.
    unsafe { *ptr::addr_of!(TEST_NAME) }
}

fn current_test_ppi27_mode() -> TestPpi27Mode {
    // SAFETY: single-core test init sets the harness mode before any handler can read it.
    unsafe { *ptr::addr_of!(TEST_PPI27_MODE) }
}

fn record_fail(code: u32) {
    let shared = shared_mut_static();
    if shared.fail == 0 {
        shared.fail = code;
    }
}

#[inline]
fn is_test_ctrl_addr(addr: usize) -> bool {
    (TEST_CTRL_BASE..TEST_CTRL_BASE + TEST_CTRL_SIZE).contains(&addr)
}

fn set_test_physical_ppi27(level: bool) -> Result<(), GicError> {
    VGIC.handle_physical_irq(gic_ref(), PIntId(TIMER_TEST_PPI_INTID), level)
}

fn inject_test_physical_ppi27() -> Result<(), GicError> {
    set_test_physical_ppi27(true)
}

fn clear_test_physical_ppi27() -> Result<(), GicError> {
    set_test_physical_ppi27(false)
}

fn set_test_physical_ppi27_or_panic(level: bool, context: &'static str) {
    let result = if level {
        inject_test_physical_ppi27()
    } else {
        clear_test_physical_ppi27()
    };

    match result {
        Ok(()) => {}
        Err(GicError::UnsupportedIntId) => {
            record_fail(FAIL_EL2_IRQ_UNSUPPORTED);
            panic!(
                "{}: unsupported physical test PPI {}",
                context, TIMER_TEST_PPI_INTID
            );
        }
        Err(err) => {
            record_fail(FAIL_EL2_IRQ_HANDLE);
            println!(
                "{}: physical irq injection failed (intid={}): {:?}",
                context, TIMER_TEST_PPI_INTID, err
            );
            panic!("{}: physical irq injection failed", context);
        }
    }
}

fn inject_test_physical_ppi27_or_panic(context: &'static str) {
    set_test_physical_ppi27_or_panic(true, context);
}

fn clear_test_physical_ppi27_or_panic(context: &'static str) {
    set_test_physical_ppi27_or_panic(false, context);
}

fn observe_asserted_test_physical_ppi27_or_panic(context: &'static str) {
    match VGIC.handle_physical_irq_asserted(gic_ref(), PIntId(TIMER_TEST_PPI_INTID)) {
        Ok(()) => {}
        Err(GicError::UnsupportedIntId) => {
            record_fail(FAIL_EL2_IRQ_UNSUPPORTED);
            panic!(
                "{}: unsupported physical test PPI {}",
                context, TIMER_TEST_PPI_INTID
            );
        }
        Err(err) => {
            record_fail(FAIL_EL2_IRQ_HANDLE);
            println!(
                "{}: physical asserted-ingress failed (intid={}): {:?}",
                context, TIMER_TEST_PPI_INTID, err
            );
            panic!("{}: physical asserted-ingress failed", context);
        }
    }
}

fn set_test_physical_ppi27_pending(pending: bool) {
    let reg_off = if pending {
        GICD_ISPENDR0_OFF
    } else {
        GICD_ICPENDR0_OFF
    };
    mmio_write32(GICD_BASE + reg_off, 1u32 << TIMER_TEST_PPI_INTID);
    cpu::dsb_sy();
    cpu::isb();
}

fn assert_test_physical_ppi27_pending() {
    set_test_physical_ppi27_pending(true);
}

fn clear_test_physical_ppi27_pending() {
    set_test_physical_ppi27_pending(false);
}

fn quiesce_test_physical_ppi27_source_or_panic() {
    match current_test_ppi27_mode() {
        TestPpi27Mode::DirectVgicSticky => {
            clear_test_physical_ppi27_or_panic("test control sticky disable");
        }
        TestPpi27Mode::RpiBootLikeSticky => {
            clear_test_physical_ppi27_or_panic("test control sticky disable");
            clear_test_physical_ppi27_pending();
        }
        TestPpi27Mode::RpiBootLikeHwLrDropOnlyNoDeactivate => {
            clear_test_physical_ppi27_or_panic("test control sticky disable");
            clear_test_physical_ppi27_pending();
        }
    }
}

fn reassert_test_physical_ppi27_source_or_panic() {
    match current_test_ppi27_mode() {
        TestPpi27Mode::DirectVgicSticky => {
            clear_test_physical_ppi27_or_panic("trapped wf sticky ppi27 clear");
            inject_test_physical_ppi27_or_panic("trapped wf sticky ppi27");
        }
        TestPpi27Mode::RpiBootLikeSticky => {
            clear_test_physical_ppi27_or_panic("trapped wf rpi_boot sticky ppi27 clear");
            clear_test_physical_ppi27_pending();
            assert_test_physical_ppi27_pending();
        }
        TestPpi27Mode::RpiBootLikeHwLrDropOnlyNoDeactivate => {}
    }
}

fn handle_test_ctrl_command(cmd: u32) {
    match cmd {
        TEST_CTRL_CMD_PPI27_STICKY_ENABLE => {
            // SAFETY: single-core EL2 test mutates this synthetic source latch in one place.
            unsafe {
                *ptr::addr_of_mut!(TEST_PPI27_STICKY_ACTIVE) = true;
            }
        }
        TEST_CTRL_CMD_PPI27_STICKY_DISABLE => {
            // SAFETY: single-core EL2 test mutates this synthetic source latch in one place.
            unsafe {
                *ptr::addr_of_mut!(TEST_PPI27_STICKY_ACTIVE) = false;
            }
            quiesce_test_physical_ppi27_source_or_panic();
        }
        _ => {
            record_fail(FAIL_EL2_TEST_CTRL_UNSUPPORTED_CMD);
            panic!("unsupported test control command 0x{cmd:08x}");
        }
    }
}

fn handle_test_ctrl_data_abort(regs: &mut cpu::Registers, info: &DataAbortInfo, addr: usize) {
    let Some(access) = info.access else {
        record_fail(FAIL_EL2_TEST_CTRL_BAD_ACCESS);
        panic!("missing test-control access metadata at 0x{:x}", addr);
    };

    if access.access_width != SyndromeAccessSize::Word
        || !matches!(access.reg_size, InstructionRegisterSize::Instruction32bit)
        || !matches!(access.write_access, WriteNotRead::WritingMemoryAbort)
    {
        record_fail(FAIL_EL2_TEST_CTRL_BAD_ACCESS);
        panic!(
            "unsupported test-control access width={:?} reg_size={:?} write_access={:?} at 0x{:x}",
            access.access_width, access.reg_size, access.write_access, addr
        );
    }

    let offset = addr - TEST_CTRL_BASE;
    if offset != TEST_CTRL_CMD_OFF {
        record_fail(FAIL_EL2_TEST_CTRL_BAD_ACCESS);
        panic!("unexpected test-control offset 0x{:x}", offset);
    }

    let Some(reg) = info.register_mut(regs) else {
        record_fail(FAIL_EL2_TEST_CTRL_BAD_ACCESS);
        panic!("invalid test-control register index {}", access.reg_num);
    };

    let cmd = (*reg & (u32::MAX as u64)) as u32;
    handle_test_ctrl_command(cmd);
}

fn el2_data_abort_handler(
    regs: &mut cpu::Registers,
    info: &DataAbortInfo,
    _decoded: Option<&exceptions::emulation::MmioDecoded>,
) {
    let addr = info.fault_ipa.unwrap_or(info.far_el2) as usize;

    if (GICD_BASE..GICD_BASE + GICV2_GICD_FRAME_SIZE).contains(&addr) {
        let Some(access) = info.access else {
            record_fail(FAIL_EL2_DABORT_NO_ACCESS);
            panic!("missing data-abort access metadata at 0x{:x}", addr);
        };

        if access.access_width != SyndromeAccessSize::Word {
            record_fail(FAIL_EL2_DABORT_BAD_WIDTH);
            panic!(
                "unsupported trapped GICD access width {:?} at 0x{:x}",
                access.access_width, addr
            );
        }

        let Some(reg) = info.register_mut(regs) else {
            record_fail(FAIL_EL2_DABORT_BAD_REGISTER);
            panic!("invalid trapped register index {}", access.reg_num);
        };

        let offset = (addr - GICD_BASE) as u32;
        match access.write_access {
            WriteNotRead::ReadingMemoryAbort => {
                let value = VGIC
                    .handle_distributor_read(VcpuId(0), dist_id(), offset, Gicv2AccessSize::U32)
                    .unwrap_or_else(|err| {
                        record_fail(FAIL_EL2_DABORT_READ);
                        panic!("vgic dist read failed at 0x{:x}: {:?}", addr, err);
                    });
                *reg = match access.reg_size {
                    InstructionRegisterSize::Instruction32bit => (value as u64) & (u32::MAX as u64),
                    InstructionRegisterSize::Instruction64bit => value as u64,
                };
            }
            WriteNotRead::WritingMemoryAbort => {
                let value = match access.reg_size {
                    InstructionRegisterSize::Instruction32bit => *reg & (u32::MAX as u64),
                    InstructionRegisterSize::Instruction64bit => *reg,
                } as u32;
                if let Err(err) = VGIC.handle_distributor_write(
                    gic_ref(),
                    VcpuId(0),
                    dist_id(),
                    offset,
                    Gicv2AccessSize::U32,
                    value,
                ) {
                    record_fail(FAIL_EL2_DABORT_WRITE);
                    panic!("vgic dist write failed at 0x{:x}: {:?}", addr, err);
                }
            }
        }
    } else if is_test_ctrl_addr(addr) {
        handle_test_ctrl_data_abort(regs, info, addr);
    } else {
        record_fail(FAIL_EL2_DABORT_BAD_ADDR);
        panic!("unexpected data abort at IPA/FAR 0x{:x}", addr);
    }

    cpu::set_elr_el2(cpu::get_elr_el2().wrapping_add(4));
}

fn el2_irq_handler(_regs: &mut cpu::Registers) {
    let gic = gic_ref();

    let ack = match gic.acknowledge() {
        Ok(Some(ack)) => ack,
        Ok(None) => return,
        Err(err) => {
            record_fail(FAIL_EL2_IRQ_HANDLE);
            panic!("gic acknowledge failed: {:?}", err);
        }
    };

    let intid = ack.intid;
    let mut panic_after_eoi: Option<&'static str> = None;
    let mut needs_deactivate = false;

    if intid == MAINT_INTID {
        if let Err(err) = VGIC.handle_maintenance(gic, VcpuId(0)) {
            record_fail(FAIL_EL2_IRQ_HANDLE);
            println!("maintenance handling failed: {:?}", err);
            panic_after_eoi = Some("maintenance handling failed");
        }
    } else if intid == KICK_INTID {
        if let Err(err) = VGIC.refill_vcpu(gic, VcpuId(0)) {
            record_fail(FAIL_EL2_IRQ_HANDLE);
            println!("kick refill failed: {:?}", err);
            panic_after_eoi = Some("kick refill failed");
        }
    } else if intid == UART_SPI_INTID {
        // SAFETY: single-core test state. Latching avoids level-triggered UART
        // storms from starving EL1 while still validating one end-to-end inject.
        let should_inject = unsafe {
            let injected = ptr::addr_of_mut!(UART_PHYS_INJECTED);
            if *injected {
                false
            } else {
                *injected = true;
                true
            }
        };
        if should_inject {
            match VGIC.handle_physical_irq(gic, PIntId(intid), true) {
                Ok(()) => {}
                Err(GicError::UnsupportedIntId) => {
                    record_fail(FAIL_EL2_IRQ_UNSUPPORTED);
                }
                Err(err) => {
                    record_fail(FAIL_EL2_IRQ_HANDLE);
                    println!("physical irq injection failed (intid={}): {:?}", intid, err);
                    panic_after_eoi = Some("physical irq injection failed");
                }
            }

            // Best-effort quiesce of the physical source after one injection.
            let mut imsc = mmio_read32(UART_BASE + UART_IMSC_OFF);
            imsc &= !UART_IMSC_TXIM;
            mmio_write32(UART_BASE + UART_IMSC_OFF, imsc);
            mmio_write32(UART_BASE + UART_ICR_OFF, UART_ICR_ALL);
        }
    } else {
        let ppi27_mode = current_test_ppi27_mode();
        let use_asserted_path = intid == TIMER_TEST_PPI_INTID
            && matches!(
                ppi27_mode,
                TestPpi27Mode::RpiBootLikeSticky
                    | TestPpi27Mode::RpiBootLikeHwLrDropOnlyNoDeactivate
            );
        let result = if use_asserted_path {
            VGIC.handle_physical_irq_asserted(gic, PIntId(intid))
        } else {
            VGIC.handle_physical_irq(gic, PIntId(intid), true)
        };

        match result {
            Ok(()) => {
                needs_deactivate =
                    intid == TIMER_TEST_PPI_INTID && ppi27_mode == TestPpi27Mode::RpiBootLikeSticky;
            }
            Err(GicError::UnsupportedIntId) => {
                record_fail(FAIL_EL2_IRQ_UNSUPPORTED);
            }
            Err(err) => {
                record_fail(FAIL_EL2_IRQ_HANDLE);
                println!("physical irq injection failed (intid={}): {:?}", intid, err);
                panic_after_eoi = Some("physical irq injection failed");
            }
        }
    }

    if let Err(err) = gic.end_of_interrupt(ack) {
        record_fail(FAIL_EL2_IRQ_EOI);
        panic!("physical EOI failed for intid {}: {:?}", intid, err);
    }

    if needs_deactivate {
        if let Err(err) = gic.deactivate(ack) {
            record_fail(FAIL_EL2_IRQ_DEACT);
            panic!("physical deactivate failed for intid {}: {:?}", intid, err);
        }
    }

    if let Some(msg) = panic_after_eoi {
        panic!("{}", msg);
    }
}

fn el2_trapped_wf_handler(_regs: &mut cpu::Registers, info: &TrappedWfInfo) {
    cpu::set_elr_el2(cpu::get_elr_el2().wrapping_add(4));

    let shared = shared_static();
    if shared.done != 0 {
        if shared.fail == 0 {
            println!("{}: PASS", current_test_name());
            exit_success();
        }

        println!(
            "{}: FAIL code=0x{:08x} last_intid={} unexpected={} last_iar=0x{:08x}",
            current_test_name(),
            shared.fail,
            shared.last_intid,
            shared.unexpected_intid,
            shared.last_iar_raw
        );
        exit_failure();
    }

    let sticky_active = {
        // SAFETY: single-core EL2 test reads this synthetic source latch without races.
        unsafe { *ptr::addr_of!(TEST_PPI27_STICKY_ACTIVE) }
    };
    if sticky_active {
        match current_test_ppi27_mode() {
            TestPpi27Mode::DirectVgicSticky | TestPpi27Mode::RpiBootLikeSticky => {
                reassert_test_physical_ppi27_source_or_panic();
                return;
            }
            TestPpi27Mode::RpiBootLikeHwLrDropOnlyNoDeactivate => {
                observe_asserted_test_physical_ppi27_or_panic(
                    "trapped wf rpi_boot persistent ppi27",
                );
                return;
            }
        }
    }

    match info.ti {
        TI::WFE | TI::WFET => {
            // SAFETY: WFE emulates guest wait semantics when trapped.
            unsafe {
                asm!("wfe", options(nomem, nostack, preserves_flags));
            }
        }
        TI::WFI | TI::WFIT => {
            // SAFETY: WFI emulates guest wait semantics when trapped.
            unsafe {
                asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }
    }
}

pub fn shared_ptr() -> *mut Shared {
    // SAFETY: raw pointer publication is safe; callers must uphold aliasing rules when dereferencing.
    ptr::addr_of_mut!(SHARED)
}

pub fn shared_set_done(shared: *mut Shared, done: u32) {
    // SAFETY: shared points at the EL2-published Shared region and write is 32-bit aligned.
    unsafe {
        ptr::write_volatile(ptr::addr_of_mut!((*shared).done), done);
    }
}

pub fn shared_set_fail_if_unset(shared: *mut Shared, code: u32) {
    // SAFETY: shared points at the EL2-published Shared region and accesses are 32-bit aligned.
    unsafe {
        let fail_ptr = ptr::addr_of_mut!((*shared).fail);
        if ptr::read_volatile(fail_ptr) == 0 {
            ptr::write_volatile(fail_ptr, code);
        }
    }
}

pub fn shared_read_fail(shared: *mut Shared) -> u32 {
    // SAFETY: shared points at the EL2-published Shared region and read is 32-bit aligned.
    unsafe { ptr::read_volatile(ptr::addr_of!((*shared).fail)) }
}

pub fn shared_set_last_intid(shared: *mut Shared, intid: u32) {
    // SAFETY: shared points at the EL2-published Shared region and write is 32-bit aligned.
    unsafe {
        ptr::write_volatile(ptr::addr_of_mut!((*shared).last_intid), intid);
    }
}

pub fn shared_set_unexpected_intid(shared: *mut Shared, intid: u32) {
    // SAFETY: shared points at the EL2-published Shared region and write is 32-bit aligned.
    unsafe {
        ptr::write_volatile(ptr::addr_of_mut!((*shared).unexpected_intid), intid);
    }
}

pub fn shared_set_last_iar_raw(shared: *mut Shared, raw: u32) {
    // SAFETY: shared points at the EL2-published Shared region and write is 32-bit aligned.
    unsafe {
        ptr::write_volatile(ptr::addr_of_mut!((*shared).last_iar_raw), raw);
    }
}

pub fn shared_increment_poll_seen(shared: *mut Shared, idx: usize) {
    // SAFETY: shared points at the EL2-published Shared region and idx is validated by callers.
    unsafe {
        let ptr = ptr::addr_of_mut!((*shared).poll_seen[idx]);
        let cur = ptr::read_volatile(ptr);
        ptr::write_volatile(ptr, cur.wrapping_add(1));
    }
}

pub fn shared_increment_irq_seen(shared: *mut Shared, idx: usize) {
    // SAFETY: shared points at the EL2-published Shared region and idx is validated by callers.
    unsafe {
        let ptr = ptr::addr_of_mut!((*shared).irq_seen[idx]);
        let cur = ptr::read_volatile(ptr);
        ptr::write_volatile(ptr, cur.wrapping_add(1));
    }
}

pub fn shared_read_irq_seen(shared: *mut Shared, idx: usize) -> u32 {
    // SAFETY: shared points at the EL2-published Shared region and idx is validated by callers.
    unsafe { ptr::read_volatile(ptr::addr_of!((*shared).irq_seen[idx])) }
}

pub fn guest_fail(shared: *mut Shared, code: u32) -> ! {
    let gic = GuestGic::default_layout();
    guest_uart_disable_tx_irq();
    gic.gicv_write32(GICV_CTLR_OFF, 0);
    gic.gicd_write32(GICD_CTLR_OFF, 0);
    cpu::dsb_sy();
    cpu::isb();
    shared_set_fail_if_unset(shared, code);
    shared_set_done(shared, 1);
    guest_wfi_loop()
}

pub fn guest_finish(shared: *mut Shared) -> ! {
    let gic = GuestGic::default_layout();
    guest_uart_disable_tx_irq();
    gic.gicv_write32(GICV_CTLR_OFF, 0);
    gic.gicd_write32(GICD_CTLR_OFF, 0);
    cpu::dsb_sy();
    cpu::isb();
    shared_set_done(shared, 1);
    guest_wfi_loop()
}

fn guest_wfi_loop() -> ! {
    loop {
        // SAFETY: guest uses WFI as a completion signal and trap point for EL2.
        unsafe {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

pub struct GuestGic {
    pub gicd_base: usize,
    pub gicv_base: usize,
}

impl GuestGic {
    pub const fn new(gicd_base: usize, gicv_base: usize) -> Self {
        Self {
            gicd_base,
            gicv_base,
        }
    }

    pub const fn default_layout() -> Self {
        Self::new(GICD_BASE, GICV_BASE)
    }

    pub fn gicd_read32(&self, offset: usize) -> u32 {
        mmio_read32(self.gicd_base + offset)
    }

    pub fn gicd_write32(&self, offset: usize, value: u32) {
        mmio_write32(self.gicd_base + offset, value);
    }

    pub fn gicv_read32(&self, offset: usize) -> u32 {
        mmio_read32(self.gicv_base + offset)
    }

    pub fn gicv_write32(&self, offset: usize, value: u32) {
        mmio_write32(self.gicv_base + offset, value);
    }
}

fn mmio_read32(addr: usize) -> u32 {
    // SAFETY: caller provides a valid 32-bit MMIO address for read access.
    unsafe { ptr::read_volatile(addr as *const u32) }
}

fn mmio_write32(addr: usize, value: u32) {
    // SAFETY: caller provides an address intended for a 32-bit MMIO-style or trapped write access.
    unsafe { ptr::write_volatile(addr as *mut u32, value) }
}

pub fn guest_test_ctrl_write(cmd: u32) {
    mmio_write32(TEST_CTRL_BASE + TEST_CTRL_CMD_OFF, cmd);
    cpu::dsb_sy();
    cpu::isb();
}

pub fn guest_enable_sticky_physical_ppi27() {
    guest_test_ctrl_write(TEST_CTRL_CMD_PPI27_STICKY_ENABLE);
}

pub fn guest_disable_sticky_physical_ppi27() {
    guest_test_ctrl_write(TEST_CTRL_CMD_PPI27_STICKY_DISABLE);
}

pub fn guest_init_virtual_interfaces(gic: &GuestGic) {
    gic.gicv_write32(GICV_PMR_OFF, 0xff);
    gic.gicv_write32(GICV_BPR_OFF, 0);
    // Group1-only delivery with AckCtl=1 so guest can use IAR/EOIR for all virtual IRQs.
    gic.gicv_write32(GICV_CTLR_OFF, 0x6);
    gic.gicd_write32(GICD_CTLR_OFF, 0x2);
    cpu::dsb_sy();
    cpu::isb();
}

pub fn guest_set_group1_intid(gic: &GuestGic, intid: u32) {
    let reg_off = GICD_IGROUPR0_OFF + ((intid as usize / 32) * 4);
    let bit = 1u32 << (intid % 32);
    let value = gic.gicd_read32(reg_off) | bit;
    gic.gicd_write32(reg_off, value);
}

pub fn guest_enable_intid(gic: &GuestGic, intid: u32) {
    let reg_off = GICD_ISENABLER0_OFF + ((intid as usize / 32) * 4);
    let bit = 1u32 << (intid % 32);
    gic.gicd_write32(reg_off, bit);
}

pub fn guest_disable_intid(gic: &GuestGic, intid: u32) {
    let reg_off = GICD_ICENABLER0_OFF + ((intid as usize / 32) * 4);
    let bit = 1u32 << (intid % 32);
    gic.gicd_write32(reg_off, bit);
}

pub fn guest_set_pending_intid(gic: &GuestGic, intid: u32) {
    let reg_off = GICD_ISPENDR0_OFF + ((intid as usize / 32) * 4);
    let bit = 1u32 << (intid % 32);
    gic.gicd_write32(reg_off, bit);
}

pub fn guest_clear_pending_intid(gic: &GuestGic, intid: u32) {
    let reg_off = GICD_ICPENDR0_OFF + ((intid as usize / 32) * 4);
    let bit = 1u32 << (intid % 32);
    gic.gicd_write32(reg_off, bit);
}

pub fn guest_send_sgi_self(gic: &GuestGic, sgi_id: u32) {
    let value = (sgi_id & 0xF) | (0b10 << 24);
    gic.gicd_write32(GICD_SGIR_OFF, value);
}

pub fn guest_configure_spi(gic: &GuestGic, intid: u32, priority: u8, target_mask: u8) {
    guest_set_group1_intid(gic, intid);
    set_gicd_byte(gic, GICD_IPRIORITYR_OFF, intid, priority);
    // Force a route transition so the vGIC route hook always runs even when
    // the default route already matches `target_mask`.
    set_gicd_byte(gic, GICD_ITARGETSR_OFF, intid, 0);
    set_gicd_byte(gic, GICD_ITARGETSR_OFF, intid, target_mask);
    // Force an enable transition so pIRQ passthrough hooks always refresh the
    // physical SPI enable state.
    guest_disable_intid(gic, intid);
    guest_enable_intid(gic, intid);
}

fn set_gicd_byte(gic: &GuestGic, base_off: usize, intid: u32, value: u8) {
    let reg_off = base_off + ((intid as usize / 4) * 4);
    let shift = ((intid % 4) * 8) as u32;
    let mut reg = gic.gicd_read32(reg_off);
    reg &= !(0xff << shift);
    reg |= (value as u32) << shift;
    gic.gicd_write32(reg_off, reg);
}

pub fn guest_uart_enable_tx_irq() {
    let mut cr = mmio_read32(UART_BASE + UART_CR_OFF);
    cr |= UART_CR_UARTEN | UART_CR_TXE | UART_CR_RXE;
    mmio_write32(UART_BASE + UART_CR_OFF, cr);

    mmio_write32(UART_BASE + UART_ICR_OFF, UART_ICR_ALL);

    let mut imsc = mmio_read32(UART_BASE + UART_IMSC_OFF);
    imsc |= UART_IMSC_TXIM;
    mmio_write32(UART_BASE + UART_IMSC_OFF, imsc);

    mmio_write32(UART_BASE + UART_DR_OFF, b'V' as u32);
}

pub fn guest_uart_disable_tx_irq() {
    let mut imsc = mmio_read32(UART_BASE + UART_IMSC_OFF);
    imsc &= !UART_IMSC_TXIM;
    mmio_write32(UART_BASE + UART_IMSC_OFF, imsc);
    mmio_write32(UART_BASE + UART_ICR_OFF, UART_ICR_ALL);
}

pub fn guest_poll_for_intid(
    shared: *mut Shared,
    gic: &GuestGic,
    expected_intid: u32,
    timeout_iters: usize,
    timeout_fail: u32,
    unexpected_fail: u32,
) -> u32 {
    for _ in 0..timeout_iters {
        let raw = guest_ack_virtual_irq(gic);
        let intid = guest_acked_intid(raw);

        if intid == expected_intid {
            shared_set_last_intid(shared, intid);
            shared_set_last_iar_raw(shared, raw);
            return raw;
        }

        if is_spurious_intid(intid) {
            cpu::isb();
            continue;
        }

        shared_set_unexpected_intid(shared, intid);
        shared_set_last_iar_raw(shared, raw);
        guest_eoi_virtual_irq(gic, raw);
        guest_fail(shared, unexpected_fail);
    }

    guest_fail(shared, timeout_fail)
}

pub fn guest_assert_not_redelivered(
    shared: *mut Shared,
    gic: &GuestGic,
    forbidden_intid: u32,
    check_iters: usize,
    duplicate_fail: u32,
) {
    for _ in 0..check_iters {
        let raw = guest_ack_virtual_irq(gic);
        let intid = guest_acked_intid(raw);

        if is_spurious_intid(intid) {
            cpu::isb();
            continue;
        }

        gic.gicv_write32(GICV_EOIR_OFF, raw);
        shared_set_unexpected_intid(shared, intid);
        shared_set_last_iar_raw(shared, raw);

        if intid == forbidden_intid {
            guest_fail(shared, duplicate_fail);
        }

        guest_fail(shared, duplicate_fail);
    }
}

pub fn guest_assert_only_spurious(
    shared: *mut Shared,
    gic: &GuestGic,
    check_iters: usize,
    unexpected_fail: u32,
) {
    for _ in 0..check_iters {
        let raw = guest_ack_virtual_irq(gic);
        let intid = guest_acked_intid(raw);

        if is_spurious_intid(intid) {
            cpu::isb();
            continue;
        }

        guest_eoi_virtual_irq(gic, raw);
        shared_set_unexpected_intid(shared, intid);
        shared_set_last_iar_raw(shared, raw);
        guest_fail(shared, unexpected_fail);
    }
}

#[inline]
fn is_spurious_intid(intid: u32) -> bool {
    intid == ALT_SPURIOUS_INTID || intid == SPURIOUS_INTID
}

pub fn guest_wait_for_irq_count(
    shared: *mut Shared,
    idx: usize,
    expected: u32,
    timeout_iters: usize,
    timeout_fail: u32,
) {
    for _ in 0..timeout_iters {
        if shared_read_irq_seen(shared, idx) >= expected {
            return;
        }

        let fail = shared_read_fail(shared);
        if fail != 0 {
            guest_fail(shared, fail);
        }

        cpu::isb();
    }

    guest_fail(shared, timeout_fail);
}

pub fn guest_wait_for_irq_count_wfe(
    shared: *mut Shared,
    idx: usize,
    expected: u32,
    timeout_iters: usize,
    timeout_fail: u32,
) {
    for _ in 0..timeout_iters {
        if shared_read_irq_seen(shared, idx) >= expected {
            return;
        }

        let fail = shared_read_fail(shared);
        if fail != 0 {
            guest_fail(shared, fail);
        }

        cpu::isb();
        // SAFETY: guest waits via trapped WFE so EL2 can model the persistent physical source.
        // `sevl; wfe; wfe` clears any stale event-latch before the trapping wait.
        unsafe {
            asm!(
                "sevl",
                "wfe",
                "wfe",
                options(nomem, nostack, preserves_flags)
            );
        }

        if shared_read_irq_seen(shared, idx) >= expected {
            return;
        }

        let fail = shared_read_fail(shared);
        if fail != 0 {
            guest_fail(shared, fail);
        }

        // SAFETY: WFI is a deterministic trapped wait fallback when WFE does not block.
        unsafe {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }

    guest_fail(shared, timeout_fail);
}

pub fn guest_ack_virtual_irq(gic: &GuestGic) -> u32 {
    gic.gicv_read32(GICV_IAR_OFF)
}

pub fn guest_eoi_virtual_irq(gic: &GuestGic, iar_raw: u32) {
    gic.gicv_write32(GICV_EOIR_OFF, iar_raw);
}

#[inline]
pub fn guest_acked_intid(iar_raw: u32) -> u32 {
    iar_raw & 0x3ff
}
