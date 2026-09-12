//! RP1 GEM ARP and optional TFTP-RRQ hardware smoke example.

#![no_std]
#![no_main]

use arch_hal::soc::bcm2712;
use arch_hal::soc::bcm2712::rp1_gem::Rp1Gem;
use arch_hal::soc::bcm2712::rp1_gem::Rp1GemOptions;
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::arch::asm;
use core::arch::naked_asm;
use core::fmt;
use core::panic::PanicInfo;
use core::ptr::null_mut;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use dtb::DtbParser;
use io_api::ethernet::EthernetFrameIo;
use io_api::ethernet::MacAddr;
use net::Ipv4Addr;
use net::arp;
use net::eth;
use net::tftp;

const DTB_PTR: usize = 0x2000_0000;
const SEMIHOST_SYS_WRITE: u64 = 0x05;
const FRAME_CAPACITY: usize = 1536;
const CLIENT_UDP_PORT: u16 = 49_152;
const POLL_TIMEOUT_US: u64 = 2_000_000;
const POLL_ATTEMPTS: usize = 1_000_000;
const HEAP_SIZE: usize = 1024 * 1024;

// Keep these board/network-specific values together so source changes are not
// required in the protocol implementation for a different lab network.
const LOCAL_MAC: MacAddr = MacAddr([0x02, 0x52, 0x50, 0x31, 0x00, 0x01]);
const LOCAL_IP: Ipv4Addr = [192, 0, 2, 10];
const SERVER_IP: Ipv4Addr = [192, 0, 2, 1];
const TFTP_FILENAME: Option<&str> = Some("BCM2712.img");

unsafe extern "C" {
    static mut _BSS_START: usize;
    static mut _BSS_END: usize;
    static mut _STACK_TOP: usize;
}

#[repr(align(16))]
struct Heap([u8; HEAP_SIZE]);

static mut HEAP: Heap = Heap([0; HEAP_SIZE]);
static HEAP_NEXT: AtomicUsize = AtomicUsize::new(0);

struct BumpAllocator;

#[global_allocator]
static GLOBAL_ALLOCATOR: BumpAllocator = BumpAllocator;

// SAFETY: allocation state is synchronized by the atomic bump offset and the
// backing storage is a dedicated static range for this example.
unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            return layout.align() as *mut u8;
        }
        let align_mask = layout.align().saturating_sub(1);
        let mut current = HEAP_NEXT.load(Ordering::Relaxed);
        loop {
            let aligned = (current + align_mask) & !align_mask;
            let Some(next) = aligned.checked_add(layout.size()) else {
                return null_mut();
            };
            if next > HEAP_SIZE {
                return null_mut();
            }
            match HEAP_NEXT.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // SAFETY: the allocation range was bounds-checked above;
                    // `HEAP` is the dedicated backing store for this allocator.
                    let base = unsafe { core::ptr::addr_of_mut!(HEAP.0).cast::<u8>() };
                    // SAFETY: `aligned` was calculated within the checked heap range.
                    return unsafe { base.add(aligned) };
                }
                Err(observed) => current = observed,
            }
        }
    }

    // SAFETY: this monotonic allocator never reclaims memory during the short
    // single-image smoke run, so deallocation deliberately has no effect.
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

struct Clock {
    ticks_per_us: u64,
}

impl Clock {
    fn new() -> Self {
        Self {
            ticks_per_us: core::cmp::max(1, timer::read_counter_frequency() / 1_000_000),
        }
    }

    fn now_us(&self) -> u64 {
        timer::read_counter() / self.ticks_per_us
    }
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    naked_asm!(
        r#"
        msr spsel, #1
        isb
        ldr x9, =_STACK_TOP
        mov sp, x9
    clear_bss:
        ldr x9, =_BSS_START
        ldr x10, =_BSS_END
    clear_bss_loop:
        cmp x9, x10
        beq clear_bss_end
        str xzr, [x9], #8
        b clear_bss_loop
    clear_bss_end:
        bl main
    loop:
        wfe
        b loop
        "#
    )
}

#[unsafe(no_mangle)]
extern "C" fn main() -> ! {
    log(format_args!("[rp1-net-smoke] start"));
    log(format_args!(
        "[rp1-net-smoke] local={:02x?} ip={}.{}.{}.{} server={}.{}.{}.{}",
        LOCAL_MAC.0,
        LOCAL_IP[0],
        LOCAL_IP[1],
        LOCAL_IP[2],
        LOCAL_IP[3],
        SERVER_IP[0],
        SERVER_IP[1],
        SERVER_IP[2],
        SERVER_IP[3],
    ));

    let dtb = match DtbParser::init(DTB_PTR) {
        Ok(dtb) => dtb,
        Err(err) => fail(format_args!("DTB parse failed: {err}")),
    };
    let rp1 = match bcm2712::init_rp1_with_options(
        &dtb,
        bcm2712::Rp1InitOptions {
            mode: bcm2712::Rp1InitMode::Auto,
            strict: false,
        },
    ) {
        Ok(rp1) => rp1,
        Err(err) => fail(format_args!("RP1 PCIe init failed: {err:?}")),
    };
    let gem = match Rp1Gem::init_from_rp1_config(&rp1, LOCAL_MAC, Rp1GemOptions::default()) {
        Ok(gem) => gem,
        Err(err) => fail(format_args!("Rp1Gem init failed: {err:?}")),
    };
    log(format_args!(
        "[rp1-net-smoke] RP1 GEM init ok PHY={}",
        gem.phy_address()
    ));

    let clock = Clock::new();
    let result = run_smoke(gem, &clock);
    match result {
        Ok(()) => log(format_args!("[rp1-net-smoke] PASS")),
        Err(reason) => {
            log(format_args!("[rp1-net-smoke] FAIL: {reason}"));
            log(format_args!(
                "[rp1-net-smoke] diagnostic={:?}",
                gem.diagnostic_snapshot()
            ));
            log(format_args!(
                "[rp1-net-smoke] last_error={:?}",
                gem.take_last_error()
            ));
        }
    }
    match gem.quiesce() {
        Ok(ncr) => log(format_args!("[rp1-net-smoke] GEM NCR stopped: {ncr:#010x}")),
        Err(err) => log(format_args!("[rp1-net-smoke] GEM stop failed: {err:?}")),
    }
    wait_forever()
}

fn run_smoke(gem: &mut Rp1Gem, clock: &Clock) -> Result<(), &'static str> {
    let server_mac = resolve_arp(gem, clock)?;
    log(format_args!(
        "[rp1-net-smoke] ARP reply {:02x?}",
        server_mac.0
    ));
    if let Some(filename) = TFTP_FILENAME {
        let mut rrq = [0u8; 512];
        let rrq_len = tftp::encode_rrq(&mut rrq, filename).map_err(|_| "RRQ encode failed")?;
        let mut frame = [0u8; FRAME_CAPACITY];
        let frame_len = net::encode_udp_ipv4_frame(
            &mut frame,
            LOCAL_MAC,
            server_mac,
            LOCAL_IP,
            SERVER_IP,
            CLIENT_UDP_PORT,
            tftp::TFTP_PORT,
            &rrq[..rrq_len],
        )
        .map_err(|_| "RRQ frame encode failed")?;
        if !gem.try_send_frame(&frame[..frame_len]) {
            return Err("RRQ transmit failed");
        }
        log(format_args!("[rp1-net-smoke] TFTP RRQ sent for {filename}"));
        log_received_packets(gem, clock, server_mac);
    }
    Ok(())
}

fn resolve_arp(gem: &mut Rp1Gem, clock: &Clock) -> Result<MacAddr, &'static str> {
    let mut request = [0u8; eth::HEADER_LEN + arp::ARP_PAYLOAD_LEN];
    let request_len = arp::encode_arp_request(&mut request, LOCAL_MAC, LOCAL_IP, SERVER_IP)
        .map_err(|_| "ARP request encode failed")?;
    if !gem.try_send_frame(&request[..request_len]) {
        return Err("ARP request transmit failed");
    }
    log(format_args!("[rp1-net-smoke] ARP request sent"));
    let start = clock.now_us();
    let mut rx = [0u8; FRAME_CAPACITY];
    for _ in 0..POLL_ATTEMPTS {
        if clock.now_us().wrapping_sub(start) >= POLL_TIMEOUT_US {
            break;
        }
        let Some(len) = gem.try_recv_frame(&mut rx) else {
            continue;
        };
        if len > rx.len() {
            continue;
        }
        if let Ok(mac) = arp::parse_arp_reply(&rx[..len], LOCAL_IP, SERVER_IP) {
            return Ok(mac);
        }
    }
    Err("ARP reply timeout")
}

fn log_received_packets(gem: &mut Rp1Gem, clock: &Clock, server_mac: MacAddr) {
    let start = clock.now_us();
    let mut rx = [0u8; FRAME_CAPACITY];
    for _ in 0..POLL_ATTEMPTS {
        if clock.now_us().wrapping_sub(start) >= POLL_TIMEOUT_US {
            log(format_args!("[rp1-net-smoke] RX timeout after RRQ"));
            return;
        }
        let Some(len) = gem.try_recv_frame(&mut rx) else {
            continue;
        };
        if len > rx.len() {
            continue;
        }
        let Ok(datagram) = net::parse_udp_ipv4_frame(&rx[..len]) else {
            continue;
        };
        if datagram.src_mac != server_mac || datagram.src_ip != SERVER_IP {
            continue;
        }
        log(format_args!(
            "[rp1-net-smoke] UDP {} -> {} bytes={}",
            datagram.src_port,
            datagram.dst_port,
            datagram.payload.len()
        ));
        if let Ok(data) = tftp::parse_data(datagram.payload) {
            log(format_args!(
                "[rp1-net-smoke] TFTP DATA block={} bytes={}",
                data.block,
                data.payload.len()
            ));
            return;
        }
        if let Ok(error) = tftp::parse_error(datagram.payload) {
            log(format_args!(
                "[rp1-net-smoke] TFTP ERROR code={}",
                error.code
            ));
            return;
        }
    }
    log(format_args!("[rp1-net-smoke] RX poll bound reached"));
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    log(format_args!("panic: {info}"));
    wait_forever()
}

fn fail(args: fmt::Arguments<'_>) -> ! {
    log(format_args!("[rp1-net-smoke] FAIL: {args}"));
    wait_forever()
}

struct SemihostWriter {
    bytes: [u8; 256],
    len: usize,
}

impl SemihostWriter {
    const fn new() -> Self {
        Self {
            bytes: [0; 256],
            len: 0,
        }
    }
}

impl fmt::Write for SemihostWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
        let target = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        target.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn log(args: fmt::Arguments<'_>) {
    let mut writer = SemihostWriter::new();
    if fmt::write(&mut writer, args).is_ok() {
        semihost_write(&writer.bytes[..writer.len]);
    } else {
        semihost_write(b"[rp1-net-smoke] log line too long");
    }
    semihost_write(b"\n");
}

fn semihost_write(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let args = [1_u64, bytes.as_ptr() as u64, bytes.len() as u64];
    // SAFETY: This is the AArch64 semihosting SYS_WRITE ABI. OpenOCD handles
    // the HLT while attached, and both stack-backed arguments live for the call.
    unsafe {
        asm!(
            "hlt #0xf000",
            inlateout("x0") SEMIHOST_SYS_WRITE => _,
            in("x1") args.as_ptr() as u64,
            options(nostack),
        );
    }
}

fn wait_forever() -> ! {
    loop {
        // SAFETY: WFE is used only as a low-power idle instruction after the
        // bounded smoke operation has completed or failed.
        unsafe {
            asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}
