use crate::guest_mmio_allowlist_contains_range;
use crate::vbar_watch;
use arch_hal::aarch64_gdb;
use arch_hal::aarch64_mutex::RawSpinLockIrqSave;
use arch_hal::cpu;
use arch_hal::psci;
use core::fmt;
use core::fmt::Write;
use gdb_remote::WatchpointKind;

pub const MAX_IGNORES: usize = 16;
const MEMFAULT_STORM_LIMIT: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemfaultPolicy {
    Off,
    Warn,
    Trap,
    Autoskip,
}

impl MemfaultPolicy {
    fn as_str(self) -> &'static str {
        match self {
            MemfaultPolicy::Off => "off",
            MemfaultPolicy::Warn => "warn",
            MemfaultPolicy::Trap => "trap",
            MemfaultPolicy::Autoskip => "autoskip",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(MemfaultPolicy::Off),
            "warn" => Some(MemfaultPolicy::Warn),
            "trap" => Some(MemfaultPolicy::Trap),
            "autoskip" => Some(MemfaultPolicy::Autoskip),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemfaultAccess {
    Read,
    Write,
}

impl MemfaultAccess {
    fn as_char(self) -> char {
        match self {
            MemfaultAccess::Read => 'r',
            MemfaultAccess::Write => 'w',
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MemfaultInfo {
    pub addr: u64,
    pub pc: u64,
    pub kind: WatchpointKind,
    pub ipa: Option<u64>,
    pub access: MemfaultAccess,
    pub size: u8,
    pub esr: u64,
    pub far: u64,
    pub reg: Option<u8>,
}

#[derive(Clone, Copy)]
struct IgnoreEntry {
    base: u64,
    len: u64,
    valid: bool,
}

impl IgnoreEntry {
    const fn empty() -> Self {
        Self {
            base: 0,
            len: 0,
            valid: false,
        }
    }

    fn contains(&self, addr: u64) -> bool {
        let Some(end) = self.base.checked_add(self.len) else {
            return false;
        };
        addr >= self.base && addr < end
    }

    fn matches(&self, base: u64, len: u64) -> bool {
        self.valid && self.base == base && self.len == len
    }
}

struct MemfaultState {
    policy: MemfaultPolicy,
    pending: bool,
    last: Option<MemfaultInfo>,
    ignores: [IgnoreEntry; MAX_IGNORES],
    storm: MemfaultStorm,
}

impl MemfaultState {
    const fn new() -> Self {
        Self {
            policy: MemfaultPolicy::Trap,
            pending: false,
            last: None,
            ignores: [IgnoreEntry::empty(); MAX_IGNORES],
            storm: MemfaultStorm::new(),
        }
    }

    fn is_ignored(&self, addr: u64) -> bool {
        self.ignores
            .iter()
            .any(|entry| entry.valid && entry.contains(addr))
    }

    fn add_ignore(&mut self, base: u64, len: u64) -> Result<(), &'static str> {
        if len == 0 {
            return Err("bad_len");
        }
        if base.checked_add(len).is_none() {
            return Err("bad_range");
        }
        if self.ignores.iter().any(|entry| entry.matches(base, len)) {
            return Ok(());
        }
        let Some(slot) = self.ignores.iter_mut().find(|entry| !entry.valid) else {
            return Err("full");
        };
        slot.base = base;
        slot.len = len;
        slot.valid = true;
        Ok(())
    }

    fn del_ignore(&mut self, base: u64, len: u64) -> Result<(), &'static str> {
        for entry in &mut self.ignores {
            if entry.matches(base, len) {
                *entry = IgnoreEntry::empty();
                return Ok(());
            }
        }
        Err("not_found")
    }
}

#[derive(Clone, Copy)]
struct MemfaultStorm {
    page: u64,
    access: MemfaultAccess,
    count: u32,
    valid: bool,
}

impl MemfaultStorm {
    const fn new() -> Self {
        Self {
            page: 0,
            access: MemfaultAccess::Read,
            count: 0,
            valid: false,
        }
    }

    fn reset(&mut self) {
        self.valid = false;
        self.count = 0;
    }
}

#[derive(Clone, Copy)]
struct MemfaultSnapshot {
    policy: MemfaultPolicy,
    pending: bool,
    last: Option<MemfaultInfo>,
    ignores: [IgnoreEntry; MAX_IGNORES],
}

static MEMFAULT_STATE: RawSpinLockIrqSave<MemfaultState> =
    RawSpinLockIrqSave::new(MemfaultState::new());

pub struct MemfaultDecision {
    pub policy: MemfaultPolicy,
    pub pending_before: bool,
    pub storm_suppress: bool,
    pub ignored: bool,
    pub should_trap: bool,
    pub should_log: bool,
}

pub fn record_memfault(info: MemfaultInfo) -> MemfaultDecision {
    let mut guard = MEMFAULT_STATE.lock_irqsave();
    let pending_before = guard.pending;
    guard.last = Some(info);
    let ignored = guard.is_ignored(info.addr)
        || guest_mmio_allowlist_contains_range(info.addr as usize, info.size as usize);
    let policy = guard.policy;
    let mut storm_trap = false;
    let mut storm_suppress = false;
    if !ignored
        && matches!(
            policy,
            MemfaultPolicy::Warn | MemfaultPolicy::Autoskip | MemfaultPolicy::Trap
        )
    {
        let page = info.addr & !0xfff;
        let access = info.access;
        if guard.storm.valid && guard.storm.page == page && guard.storm.access == access {
            guard.storm.count = guard.storm.count.saturating_add(1);
        } else {
            guard.storm.page = page;
            guard.storm.access = access;
            guard.storm.count = 1;
            guard.storm.valid = true;
        }
        if guard.storm.count >= MEMFAULT_STORM_LIMIT {
            if policy == MemfaultPolicy::Trap {
                storm_suppress = true;
            } else {
                storm_trap = true;
            }
            guard.storm.reset();
        }
    } else {
        guard.storm.reset();
    }

    let should_trap = (policy == MemfaultPolicy::Trap && !ignored) || storm_trap;
    let should_log = !ignored
        && matches!(policy, MemfaultPolicy::Warn | MemfaultPolicy::Autoskip)
        && !storm_trap;
    let should_pending =
        !ignored && matches!(policy, MemfaultPolicy::Warn | MemfaultPolicy::Autoskip);
    if should_trap || should_pending {
        guard.pending = true;
    }
    MemfaultDecision {
        policy,
        pending_before,
        storm_suppress,
        ignored,
        should_trap,
        should_log,
    }
}

/// Enable memfault trapping once a debug session becomes active.
///
/// This ensures watchpoint-style stops even if the policy was set to `Off`.
pub fn enable_memfault_trap_if_off() {
    let mut guard = MEMFAULT_STATE.lock_irqsave();
    if guard.policy == MemfaultPolicy::Off {
        guard.policy = MemfaultPolicy::Trap;
    }
}

struct OutBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> OutBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn try_write_bytes(&mut self, bytes: &[u8]) -> fmt::Result {
        if self.buf.len().saturating_sub(self.len) < bytes.len() {
            return Err(fmt::Error);
        }
        let end = self.len + bytes.len();
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    fn try_write_str(&mut self, s: &str) -> fmt::Result {
        self.try_write_bytes(s.as_bytes())
    }

    fn force_truncated_marker(&mut self) {
        const MARKER: &[u8] = b"...TRUNCATED";
        if self.buf.len() < MARKER.len() {
            return;
        }
        let start = self.buf.len() - MARKER.len();
        self.buf[start..].copy_from_slice(MARKER);
        self.len = self.buf.len();
    }
}

impl fmt::Write for OutBuf<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let avail = self.buf.len().saturating_sub(self.len);
        let copy_len = bytes.len().min(avail);
        if copy_len > 0 {
            self.buf[self.len..self.len + copy_len].copy_from_slice(&bytes[..copy_len]);
            self.len += copy_len;
        }
        Ok(())
    }
}

fn snapshot_state() -> MemfaultSnapshot {
    let guard = MEMFAULT_STATE.lock_irqsave();
    MemfaultSnapshot {
        policy: guard.policy,
        pending: guard.pending,
        last: guard.last,
        ignores: guard.ignores,
    }
}

fn set_policy(policy: MemfaultPolicy) {
    let mut guard = MEMFAULT_STATE.lock_irqsave();
    guard.policy = policy;
    guard.pending = false;
}

fn clear_pending() {
    let mut guard = MEMFAULT_STATE.lock_irqsave();
    guard.pending = false;
}

pub fn clear_memfault_pending() {
    clear_pending();
}

fn add_ignore(base: u64, len: u64) -> Result<(), &'static str> {
    let mut guard = MEMFAULT_STATE.lock_irqsave();
    guard.add_ignore(base, len)
}

fn del_ignore(base: u64, len: u64) -> Result<(), &'static str> {
    let mut guard = MEMFAULT_STATE.lock_irqsave();
    guard.del_ignore(base, len)
}

fn add_ignore_last(len: u64) -> Result<u64, &'static str> {
    let mut guard = MEMFAULT_STATE.lock_irqsave();
    let Some(info) = guard.last else {
        return Err("no_last");
    };
    let base = info.addr;
    guard.add_ignore(base, len)?;
    Ok(base)
}

fn write_error(out: &mut OutBuf<'_>, reason: &str) {
    let _ = write!(out, "error={}", reason);
}

fn error_response(out: &mut OutBuf<'_>, reason: &str) -> Option<usize> {
    write_error(out, reason);
    Some(out.len())
}

fn memfault_kind_label(kind: WatchpointKind) -> &'static str {
    match kind {
        WatchpointKind::Read => "read",
        WatchpointKind::Write => "write",
        WatchpointKind::Access => "access",
    }
}

fn memfault_class(info: MemfaultInfo) -> &'static str {
    let Ok(addr) = usize::try_from(info.addr) else {
        return "invalid";
    };
    if guest_mmio_allowlist_contains_range(addr, info.size as usize) {
        "allowlisted"
    } else {
        "invalid"
    }
}

fn write_memfault_info(out: &mut OutBuf<'_>, info: MemfaultInfo) {
    let _ = write!(
        out,
        "addr=0x{:x} kind={} access={} size={} esr=0x{:x} elr=0x{:x}",
        info.addr,
        memfault_kind_label(info.kind),
        info.access.as_char(),
        info.size,
        info.esr,
        info.pc,
    );
    match info.ipa {
        Some(ipa) => {
            let _ = write!(out, " ipa=0x{:x}", ipa);
        }
        None => {
            let _ = write!(out, " ipa=none");
        }
    }
    match info.reg {
        Some(reg) => {
            let _ = write!(out, " far=0x{:x} reg={}", info.far, reg);
        }
        None => {
            let _ = write!(out, " far=0x{:x} reg=none", info.far);
        }
    }
    let _ = write!(out, " class={}", memfault_class(info),);
}

fn write_ignore_list(out: &mut OutBuf<'_>, ignores: &[IgnoreEntry; MAX_IGNORES]) {
    let mut count = 0usize;
    for entry in ignores.iter().filter(|entry| entry.valid) {
        let _ = entry;
        count += 1;
    }
    let _ = write!(out, "count={} entries=", count);
    let mut first = true;
    for entry in ignores.iter().filter(|entry| entry.valid) {
        if !first {
            let _ = write!(out, ",");
        }
        first = false;
        let _ = write!(out, "0x{:x}+0x{:x}", entry.base, entry.len);
    }
}

fn write_vbar_usage(out: &mut OutBuf<'_>) {
    let _ = write!(out, "usage=hp vbar <status|last|clear|check|bt?|bt>");
}

fn write_hp_help(out: &mut OutBuf<'_>) {
    const HELP_LINES: &[&str] = &[
        "HyprProbe monitor commands:\n",
        "  monitor help\n",
        "  monitor hp help\n",
        "  monitor hp memfault?\n",
        "  monitor hp memfault last|clear\n",
        "  monitor hp memfault policy get|set <off|trap|autoskip>\n",
        "  monitor hp memfault ignore add <addr> <len>\n",
        "  monitor hp memfault ignore add_last <len>\n",
        "  monitor hp memfault ignore del <addr> <len>\n",
        "  monitor hp memfault ignore list\n",
        "  monitor hp semihost?\n",
        "  monitor hp semihost info\n",
        "  monitor hp semihost read <n>\n",
        "  monitor hp semihost reply <result> <errno>\n",
        "  monitor hp semihost alloc_handle\n",
        "  monitor hp semihost reset\n",
        "  monitor hp reset\n",
        "  monitor hp gdb stop-counters\n",
        "  monitor hp vbar <status|last|clear|check|bt?|bt>\n",
    ];
    for &line in HELP_LINES {
        if out.try_write_str(line).is_err() {
            out.force_truncated_marker();
            return;
        }
    }
}

fn write_vbar_status(out: &mut OutBuf<'_>) {
    let snapshot = vbar_watch::snapshot_status();
    let live_vbar = cpu::get_vbar_el1();

    let _ = write!(
        out,
        "enabled={} mode={} current_vbar_va=0x{:x} current_vbar_ipa=0x{:x} live_vbar=0x{:x}",
        snapshot.enabled as u8,
        snapshot.mode.as_str(),
        snapshot.current_vbar_va,
        snapshot.current_vbar_ipa,
        live_vbar
    );
    let _ = write!(
        out,
        " pending_repatch={} step_depth={} change_seq={} change_reason={}",
        snapshot.pending_repatch as u8,
        snapshot.step_depth,
        snapshot.last_change_seq,
        snapshot.last_change_reason.as_str()
    );
    if live_vbar != snapshot.current_vbar_va {
        let _ = write!(
            out,
            " warning=vbar_changed pending_repatch={}",
            snapshot.pending_repatch as u8
        );
    }
    if let Some(err) = snapshot.last_error {
        let _ = write!(out, " error={} err_vbar=0x{:x}", err.reason, err.vbar_va);
    }
}

fn write_vbar_last(out: &mut OutBuf<'_>) {
    let Some(hit) = vbar_watch::last_hit_snapshot() else {
        let _ = write!(out, "none");
        return;
    };
    let mode = vbar_watch::spsr_el1_mode_label(hit.origin_spsr_el1);
    let _ = write!(
        out,
        "slot={} offset=0x{:x} brk_pc=0x{:x} esr_el2=0x{:x} elr_el2=0x{:x}",
        hit.slot_index, hit.offset, hit.elr, hit.esr, hit.elr
    );
    let _ = write!(
        out,
        " origin_pc=0x{:x} origin_spsr_el1=0x{:x} origin_mode={}",
        hit.origin_pre_pc, hit.origin_spsr_el1, mode
    );
    match hit.origin_pre_sp {
        Some(sp) => {
            let _ = write!(out, " origin_pre_sp=0x{:x}", sp);
        }
        None => {
            let _ = write!(out, " origin_pre_sp=unknown");
        }
    }
    let _ = write!(
        out,
        " origin_sp_el0=0x{:x} origin_sp_el1=0x{:x} origin_esr_el1=0x{:x} origin_far_el1=0x{:x} nested={}",
        hit.origin_sp_el0,
        hit.origin_sp_el1,
        hit.origin_esr_el1,
        hit.origin_far_el1,
        hit.nested as u8
    );
}

fn push_hex_u8(out: &mut OutBuf<'_>, b: u8) -> fmt::Result {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 2];
    buf[0] = HEX[(b >> 4) as usize];
    buf[1] = HEX[(b & 0xf) as usize];
    out.try_write_bytes(&buf)
}

fn push_hex_bytes(out: &mut OutBuf<'_>, bytes: &[u8]) -> fmt::Result {
    for &b in bytes {
        push_hex_u8(out, b)?;
    }
    Ok(())
}

fn write_vbar_bt_meta(out: &mut OutBuf<'_>) {
    let Some((seq, depth, nested)) = vbar_watch::snapshot_last_bt_meta() else {
        let _ = write!(out, "none");
        return;
    };
    let _ = write!(
        out,
        "seq={} depth={} nested={} version=1",
        seq, depth, nested as u8
    );
}

fn write_vbar_bt_dump(out: &mut OutBuf<'_>) {
    let Some((seq, depth, frames)) = vbar_watch::snapshot_last_bt_dump() else {
        let _ = write!(out, "none");
        return;
    };
    let _ = write!(
        out,
        "version=1 seq={} depth={} stride=56 fields=pc,sp,fp,lr,spsr,esr,far data=",
        seq, depth
    );
    let mut wrote_all = true;
    for frame in frames.iter().take(depth as usize) {
        let bytes = [
            frame.pc.to_le_bytes(),
            frame.sp.to_le_bytes(),
            frame.fp.to_le_bytes(),
            frame.lr.to_le_bytes(),
            frame.spsr.to_le_bytes(),
            frame.esr.to_le_bytes(),
            frame.far.to_le_bytes(),
        ];
        for chunk in bytes.iter() {
            if push_hex_bytes(out, chunk).is_err() {
                wrote_all = false;
                break;
            }
        }
        if !wrote_all {
            break;
        }
    }
    if !wrote_all {
        out.force_truncated_marker();
    }
}

fn parse_u64_token(token: &str) -> Option<u64> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        if hex.is_empty() {
            return None;
        }
        u64::from_str_radix(hex, 16).ok()
    } else {
        u64::from_str_radix(token, 10).ok()
    }
}

fn try_system_reset_via_psci() -> Result<core::convert::Infallible, i64> {
    let saved = cpu::irq_save();
    // SAFETY: cpu::Registers is a plain-old-data struct of u64 fields; all-zero is a valid initial value for staging an SMC call.
    let mut regs: cpu::Registers = unsafe { core::mem::zeroed() };
    regs.x0 = psci::PsciFunctionId::SystemReset as u64;
    psci::secure_monitor_call(&mut regs);
    cpu::irq_restore(saved);
    Err(regs.x0 as i64)
}

pub fn bootloader_monitor_handler(cmd: &[u8], out: &mut [u8]) -> Option<usize> {
    let Ok(text) = core::str::from_utf8(cmd) else {
        return None;
    };
    let mut parts = text.split_ascii_whitespace();
    let Some(root) = parts.next() else {
        return None;
    };
    if root != "hp" && root != "help" {
        return None;
    }

    let mut out = OutBuf::new(out);
    match root {
        "help" => {
            if parts.next().is_some() {
                write_error(&mut out, "extra_args");
            } else {
                write_hp_help(&mut out);
            }
            Some(out.len())
        }
        "hp" => match parts.next() {
            None => {
                write_hp_help(&mut out);
                Some(out.len())
            }
            Some("help") => {
                if parts.next().is_some() {
                    write_error(&mut out, "extra_args");
                } else {
                    write_hp_help(&mut out);
                }
                Some(out.len())
            }
            Some(area) => match area {
                "gdb" => {
                    let Some(cmd) = parts.next() else {
                        return error_response(&mut out, "bad_args");
                    };
                    match cmd {
                        "stop-counters" => {
                            if parts.next().is_some() {
                                return error_response(&mut out, "extra_args");
                            }
                            let counters = aarch64_gdb::stop_reply_counters();
                            let _ = write!(
                                out,
                                "queued={} overflow={} sent={}",
                                counters.queued, counters.overflow, counters.sent
                            );
                            Some(out.len())
                        }
                        _ => error_response(&mut out, "bad_args"),
                    }
                }
                "memfault?" => {
                    if parts.next().is_some() {
                        return error_response(&mut out, "extra_args");
                    }
                    let snapshot = snapshot_state();
                    if snapshot.pending {
                        let Some(info) = snapshot.last else {
                            let _ = write!(out, "no");
                            return Some(out.len());
                        };
                        let _ = write!(out, "yes ");
                        write_memfault_info(&mut out, info);
                        clear_pending();
                    } else {
                        let _ = write!(out, "no");
                    }
                    Some(out.len())
                }
                "memfault" => {
                    let Some(cmd) = parts.next() else {
                        return error_response(&mut out, "bad_args");
                    };
                    match cmd {
                        "last" => {
                            if parts.next().is_some() {
                                return error_response(&mut out, "extra_args");
                            }
                            let snapshot = snapshot_state();
                            if let Some(info) = snapshot.last {
                                write_memfault_info(&mut out, info);
                                let _ = write!(
                                    out,
                                    " pending={}",
                                    if snapshot.pending { 1 } else { 0 }
                                );
                            } else {
                                let _ = write!(out, "none");
                            }
                            Some(out.len())
                        }
                        "clear" => {
                            if parts.next().is_some() {
                                return error_response(&mut out, "extra_args");
                            }
                            clear_pending();
                            let _ = write!(out, "ok");
                            Some(out.len())
                        }
                        "policy" => {
                            let Some(subcmd) = parts.next() else {
                                return error_response(&mut out, "bad_args");
                            };
                            match subcmd {
                                "get" => {
                                    if parts.next().is_some() {
                                        return error_response(&mut out, "extra_args");
                                    }
                                    let snapshot = snapshot_state();
                                    let _ = write!(out, "policy={}", snapshot.policy.as_str());
                                    Some(out.len())
                                }
                                "set" => {
                                    let Some(policy_str) = parts.next() else {
                                        return error_response(&mut out, "bad_args");
                                    };
                                    if parts.next().is_some() {
                                        return error_response(&mut out, "extra_args");
                                    }
                                    let Some(policy) = MemfaultPolicy::parse(policy_str) else {
                                        return error_response(&mut out, "bad_policy");
                                    };
                                    set_policy(policy);
                                    let _ = write!(out, "ok policy={}", policy.as_str());
                                    Some(out.len())
                                }
                                _ => error_response(&mut out, "bad_args"),
                            }
                        }
                        "ignore" => {
                            let Some(subcmd) = parts.next() else {
                                return error_response(&mut out, "bad_args");
                            };
                            match subcmd {
                                action @ ("add" | "del") => {
                                    let Some(addr_str) = parts.next() else {
                                        return error_response(&mut out, "bad_args");
                                    };
                                    let Some(len_str) = parts.next() else {
                                        return error_response(&mut out, "bad_args");
                                    };
                                    if parts.next().is_some() {
                                        return error_response(&mut out, "extra_args");
                                    }
                                    let Some(base) = parse_u64_token(addr_str) else {
                                        return error_response(&mut out, "bad_addr");
                                    };
                                    let Some(len) = parse_u64_token(len_str) else {
                                        return error_response(&mut out, "bad_len");
                                    };
                                    let result = if action == "add" {
                                        add_ignore(base, len)
                                    } else {
                                        del_ignore(base, len)
                                    };
                                    match result {
                                        Ok(()) => {
                                            let _ =
                                                write!(out, "ok addr=0x{:x} len=0x{:x}", base, len);
                                        }
                                        Err(reason) => write_error(&mut out, reason),
                                    }
                                    Some(out.len())
                                }
                                "add_last" => {
                                    let Some(len_str) = parts.next() else {
                                        return error_response(&mut out, "bad_args");
                                    };
                                    if parts.next().is_some() {
                                        return error_response(&mut out, "extra_args");
                                    }
                                    let Some(len) = parse_u64_token(len_str) else {
                                        return error_response(&mut out, "bad_len");
                                    };
                                    match add_ignore_last(len) {
                                        Ok(base) => {
                                            let _ =
                                                write!(out, "ok addr=0x{:x} len=0x{:x}", base, len);
                                        }
                                        Err(reason) => write_error(&mut out, reason),
                                    }
                                    Some(out.len())
                                }
                                "list" => {
                                    if parts.next().is_some() {
                                        return error_response(&mut out, "extra_args");
                                    }
                                    let snapshot = snapshot_state();
                                    write_ignore_list(&mut out, &snapshot.ignores);
                                    Some(out.len())
                                }
                                _ => error_response(&mut out, "bad_args"),
                            }
                        }
                        _ => error_response(&mut out, "bad_args"),
                    }
                }
                "reset" => {
                    if parts.next().is_some() {
                        return error_response(&mut out, "extra_args");
                    }
                    match try_system_reset_via_psci() {
                        Ok(never) => match never {},
                        Err(rc) => {
                            let _ = write!(out, "error=psci_failed rc={}", rc);
                            Some(out.len())
                        }
                    }
                }
                "vbar" => {
                    let Some(cmd) = parts.next() else {
                        write_vbar_usage(&mut out);
                        return Some(out.len());
                    };
                    match cmd {
                        "status" => {
                            if parts.next().is_some() {
                                return error_response(&mut out, "extra_args");
                            }
                            write_vbar_status(&mut out);
                            Some(out.len())
                        }
                        "last" => {
                            if parts.next().is_some() {
                                return error_response(&mut out, "extra_args");
                            }
                            write_vbar_last(&mut out);
                            Some(out.len())
                        }
                        "clear" => {
                            if parts.next().is_some() {
                                return error_response(&mut out, "extra_args");
                            }
                            vbar_watch::clear_last_hit();
                            let _ = write!(out, "ok");
                            Some(out.len())
                        }
                        "bt?" => {
                            if parts.next().is_some() {
                                return error_response(&mut out, "extra_args");
                            }
                            write_vbar_bt_meta(&mut out);
                            Some(out.len())
                        }
                        "bt" => {
                            if parts.next().is_some() {
                                return error_response(&mut out, "extra_args");
                            }
                            write_vbar_bt_dump(&mut out);
                            Some(out.len())
                        }
                        "check" => {
                            if parts.next().is_some() {
                                return error_response(&mut out, "extra_args");
                            }
                            vbar_watch::poll_vbar_el1_change();
                            write_vbar_status(&mut out);
                            Some(out.len())
                        }
                        _ => {
                            write_vbar_usage(&mut out);
                            Some(out.len())
                        }
                    }
                }
                _ => error_response(&mut out, "bad_args"),
            },
        },
        _ => unreachable!(),
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;

    #[test_case]
    fn invalid_monitor_commands_report_exact_errors() {
        let mut out = [0u8; 64];
        for (cmd, expected) in [
            (b"hp gdb".as_slice(), b"error=bad_args".as_slice()),
            (b"hp nope", b"error=bad_args"),
            (b"hp gdb stop-counters extra", b"error=extra_args"),
            (b"hp memfault policy set nope", b"error=bad_policy"),
            (b"hp memfault ignore add nope 1", b"error=bad_addr"),
            (b"hp memfault ignore add 1 nope", b"error=bad_len"),
        ] {
            let len = bootloader_monitor_handler(cmd, &mut out).unwrap();
            assert_eq!(&out[..len], expected);
        }
    }

    #[test_case]
    fn ignore_add_and_del_share_range_parser() {
        let mut out = [0u8; 64];
        for cmd in [
            b"hp memfault ignore add 0x1b000000 0x1000".as_slice(),
            b"hp memfault ignore del 0x1b000000 0x1000".as_slice(),
        ] {
            let len = bootloader_monitor_handler(cmd, &mut out).unwrap();
            let response = core::str::from_utf8(&out[..len]).unwrap();
            assert_eq!(response, "ok addr=0x1b000000 len=0x1000");
        }
    }
}
