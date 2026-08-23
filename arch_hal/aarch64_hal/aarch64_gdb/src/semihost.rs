use aarch64_mutex::RawSpinLockIrqSave;
use core::fmt;
use core::fmt::Write;
use cpu::Registers;

use super::MemoryAccess;

const SEMIHOST_HLT_BASE: u32 = 0xD440_0000;
const SEMIHOST_HLT_MASK: u32 = 0xFFE0_001F;
const SEMIHOST_HLT_IMM: u32 = 0xF000;
const SEMIHOST_READ_MAX: usize = 4096;

const SYS_OPEN: u32 = 0x01;
const SYS_CLOSE: u32 = 0x02;
const SYS_WRITE0: u32 = 0x04;
const SYS_WRITE: u32 = 0x05;

pub const EINVAL: i32 = 22;

const O_RDONLY: u32 = 0;
const O_WRONLY: u32 = 1;
const O_CREAT: u32 = 0x40;
const O_TRUNC: u32 = 0x200;
const O_APPEND: u32 = 0x400;
const FILEIO_CREATE_MODE: u32 = 0o666;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemihostOp {
    Write0,
    Open,
    Write,
    Close,
}

#[derive(Clone, Copy, Debug)]
pub struct SemihostRequest {
    pub op: u64,
    pub args_ptr: u64,
    pub insn_addr: u64,
    pub pc_after: u64,
    pub kind: Option<SemihostOp>,
    pub handle: u64,
    pub buf_ptr: u64,
    pub len: u64,
    pub mode: u64,
    pub decoded_ok: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SemihostCompletion {
    pub result: i64,
    pub errno: i32,
}

struct SemihostState {
    pending: bool,
    req: Option<SemihostRequest>,
    completion: Option<SemihostCompletion>,
    next_handle: u32,
    read_offset: u64,
    read_done: bool,
    fileio_enabled: bool,
    fileio_inflight: bool,
}

impl SemihostState {
    const fn new() -> Self {
        Self {
            pending: false,
            req: None,
            completion: None,
            next_handle: 3,
            read_offset: 0,
            read_done: false,
            fileio_enabled: true,
            fileio_inflight: false,
        }
    }

    fn clear(&mut self) {
        self.pending = false;
        self.req = None;
        self.completion = None;
        self.read_offset = 0;
        self.read_done = false;
        self.fileio_inflight = false;
    }

    fn start_request(&mut self, req: SemihostRequest) {
        self.pending = true;
        self.req = Some(req);
        self.completion = None;
        self.read_offset = 0;
        self.read_done = false;
        self.fileio_inflight = false;
    }

    fn alloc_handle(&mut self) -> u32 {
        let mut handle = self.next_handle;
        if handle == 0 {
            handle = 1;
        }
        self.next_handle = handle.wrapping_add(1);
        if self.next_handle == 0 {
            self.next_handle = 1;
        }
        handle
    }
}

static SEMIHOST: RawSpinLockIrqSave<SemihostState> = RawSpinLockIrqSave::new(SemihostState::new());

#[derive(Clone, Copy, Debug)]
pub enum ResumeGate {
    Hold,
    Proceed(Option<(SemihostRequest, SemihostCompletion)>),
}

fn semihost_op(op: u64) -> Option<SemihostOp> {
    match (op & 0xffff_ffff) as u32 {
        SYS_WRITE0 => Some(SemihostOp::Write0),
        SYS_OPEN => Some(SemihostOp::Open),
        SYS_WRITE => Some(SemihostOp::Write),
        SYS_CLOSE => Some(SemihostOp::Close),
        _ => None,
    }
}

pub fn is_semihost_hlt(insn: u32) -> bool {
    (insn & SEMIHOST_HLT_MASK) == SEMIHOST_HLT_BASE && ((insn >> 5) & 0xffff) == SEMIHOST_HLT_IMM
}

pub fn capture_from_debug<M: MemoryAccess>(mem: &mut M, regs: &Registers, pc: u64) -> bool {
    let mut insn_addr = None;
    if let Ok(insn) = read_insn(mem, pc) {
        if is_semihost_hlt(insn) {
            insn_addr = Some(pc);
        }
    }
    if insn_addr.is_none() && pc >= 4 {
        let pc_prev = pc.wrapping_sub(4);
        if let Ok(insn) = read_insn(mem, pc_prev) {
            if is_semihost_hlt(insn) {
                insn_addr = Some(pc_prev);
            }
        }
    }

    let Some(insn_addr) = insn_addr else {
        return false;
    };

    let req = decode_request(mem, regs, insn_addr);
    let mut guard = SEMIHOST.lock_irqsave();
    guard.start_request(req);
    true
}

pub fn resume_gate() -> ResumeGate {
    let mut guard = SEMIHOST.lock_irqsave();
    if !guard.pending {
        return ResumeGate::Proceed(None);
    }
    if guard.completion.is_none() {
        return ResumeGate::Hold;
    }
    let req = guard.req;
    let completion = guard.completion;
    guard.clear();
    match (req, completion) {
        (Some(req), Some(completion)) => ResumeGate::Proceed(Some((req, completion))),
        _ => ResumeGate::Proceed(None),
    }
}

pub fn fileio_enabled() -> bool {
    let guard = SEMIHOST.lock_irqsave();
    guard.fileio_enabled
}

pub fn fileio_pending() -> bool {
    let guard = SEMIHOST.lock_irqsave();
    guard.pending && guard.completion.is_none()
}

pub fn fileio_inflight() -> bool {
    let guard = SEMIHOST.lock_irqsave();
    guard.fileio_inflight
}

pub fn fileio_clear_inflight() -> Result<(), &'static str> {
    let mut guard = SEMIHOST.lock_irqsave();
    if !guard.pending {
        return Err("no_pending");
    }
    guard.fileio_inflight = false;
    Ok(())
}

fn set_fileio_enabled(enabled: bool) {
    let mut guard = SEMIHOST.lock_irqsave();
    guard.fileio_enabled = enabled;
}

pub fn fileio_try_build_request<M: MemoryAccess>(
    mem: &mut M,
    out: &mut [u8],
) -> Result<Option<usize>, &'static str> {
    let mut guard = SEMIHOST.lock_irqsave();
    if !guard.pending || guard.completion.is_some() {
        return Ok(None);
    }
    if guard.fileio_inflight {
        return Ok(None);
    }
    let Some(req) = guard.req else {
        return Ok(None);
    };
    let Some(kind) = req.kind else {
        let _ = store_completion_locked(&mut guard, -1, EINVAL);
        return Ok(None);
    };
    if !req.decoded_ok && !matches!(kind, SemihostOp::Write0) {
        let _ = store_completion_locked(&mut guard, -1, EINVAL);
        return Ok(None);
    }

    let mut out_buf = OutBuf::new(out);
    let build = (|| -> Result<(), &'static str> {
        match kind {
            SemihostOp::Open => {
                let flags = match req.mode {
                    0 => O_RDONLY,
                    4 => O_WRONLY | O_CREAT | O_TRUNC,
                    8 => O_WRONLY | O_CREAT | O_APPEND,
                    _ => return Err("bad_mode"),
                };
                let mode = if (flags & O_CREAT) != 0 {
                    FILEIO_CREATE_MODE
                } else {
                    0
                };
                write_bytes(&mut out_buf, b"Fopen,")?;
                write_hex_u64(&mut out_buf, req.buf_ptr)?;
                write_bytes(&mut out_buf, b"/")?;
                write_hex_u64(&mut out_buf, req.len)?;
                write_bytes(&mut out_buf, b",")?;
                write_hex_u64(&mut out_buf, flags as u64)?;
                write_bytes(&mut out_buf, b",")?;
                write_hex_u64(&mut out_buf, mode as u64)?;
            }
            SemihostOp::Write => {
                write_bytes(&mut out_buf, b"Fwrite,")?;
                write_hex_u64(&mut out_buf, req.handle)?;
                write_bytes(&mut out_buf, b",")?;
                write_hex_u64(&mut out_buf, req.buf_ptr)?;
                write_bytes(&mut out_buf, b"/")?;
                write_hex_u64(&mut out_buf, req.len)?;
            }
            SemihostOp::Close => {
                write_bytes(&mut out_buf, b"Fclose,")?;
                write_hex_u64(&mut out_buf, req.handle)?;
            }
            SemihostOp::Write0 => {
                let len = match cstring_len(mem, req.buf_ptr, SEMIHOST_READ_MAX) {
                    Ok(Some(len)) => len,
                    Ok(None) => return Err("no_terminator"),
                    Err(_) => return Err("mem_read"),
                };
                write_bytes(&mut out_buf, b"Fwrite,1,")?;
                write_hex_u64(&mut out_buf, req.buf_ptr)?;
                write_bytes(&mut out_buf, b"/")?;
                write_hex_u64(&mut out_buf, len as u64)?;
            }
        }
        Ok(())
    })();
    if build.is_err() {
        let _ = store_completion_locked(&mut guard, -1, EINVAL);
        return Ok(None);
    }

    guard.fileio_inflight = true;
    Ok(Some(out_buf.len()))
}

pub fn fileio_on_reply(retcode: i64, errno: i32) -> Result<(), &'static str> {
    let mut guard = SEMIHOST.lock_irqsave();
    if !guard.pending {
        return Err("no_pending");
    }
    if !guard.fileio_inflight {
        return Err("not_inflight");
    }
    let Some(req) = guard.req else {
        guard.fileio_inflight = false;
        return Err("no_request");
    };
    let Some(kind) = req.kind else {
        let _ = store_completion_locked(&mut guard, -1, EINVAL);
        guard.fileio_inflight = false;
        return Err("unsupported_op");
    };
    if !req.decoded_ok && !matches!(kind, SemihostOp::Write0) {
        let _ = store_completion_locked(&mut guard, -1, EINVAL);
        guard.fileio_inflight = false;
        return Err("decode_failed");
    }

    let result = match kind {
        SemihostOp::Open => {
            if retcode >= 0 {
                retcode
            } else {
                -1
            }
        }
        SemihostOp::Close => {
            if retcode == 0 {
                0
            } else {
                -1
            }
        }
        SemihostOp::Write => {
            if retcode >= 0 {
                let written = retcode as u64;
                let remaining = req.len.saturating_sub(written);
                i64::try_from(remaining).unwrap_or(i64::MAX)
            } else {
                i64::try_from(req.len).unwrap_or(i64::MAX)
            }
        }
        SemihostOp::Write0 => 0,
    };

    let res = store_completion_locked(&mut guard, result, errno);
    guard.fileio_inflight = false;
    res
}

pub fn monitor_command<M: MemoryAccess>(cmd: &str, out: &mut [u8], mem: &mut M) -> Option<usize> {
    let mut parts = cmd.split_ascii_whitespace();
    let root = parts.next()?;
    if root != "hp" {
        return None;
    }
    let Some(subcmd) = parts.next() else {
        return None;
    };

    let mut out = OutBuf::new(out);
    match subcmd {
        "semihost?" => {
            if parts.next().is_some() {
                write_error(&mut out, "extra_args");
            } else {
                write_semihost_query(&mut out);
            }
            Some(out.len())
        }
        "semihost" => {
            let Some(action) = parts.next() else {
                write_error(&mut out, "bad_args");
                return Some(out.len());
            };
            match action {
                "info" => {
                    if parts.next().is_some() {
                        write_error(&mut out, "extra_args");
                    } else {
                        write_semihost_info(&mut out);
                    }
                }
                "read" => {
                    let Some(limit) = parts.next() else {
                        write_error(&mut out, "bad_args");
                        return Some(out.len());
                    };
                    if parts.next().is_some() {
                        write_error(&mut out, "extra_args");
                    } else {
                        match parse_usize_token(limit) {
                            Some(limit) => {
                                if let Err(msg) = write_semihost_read(&mut out, mem, limit) {
                                    write_error(&mut out, msg);
                                }
                            }
                            None => write_error(&mut out, "bad_len"),
                        }
                    }
                }
                "fileio?" => {
                    if parts.next().is_some() {
                        write_error(&mut out, "extra_args");
                    } else if fileio_enabled() {
                        let _ = write!(out, "fileio=on\n");
                    } else {
                        let _ = write!(out, "fileio=off\n");
                    }
                }
                "fileio" => {
                    let Some(mode) = parts.next() else {
                        write_error(&mut out, "bad_args");
                        return Some(out.len());
                    };
                    if parts.next().is_some() {
                        write_error(&mut out, "extra_args");
                    } else {
                        match mode {
                            "on" => {
                                set_fileio_enabled(true);
                                let _ = write!(out, "ok\n");
                            }
                            "off" => {
                                set_fileio_enabled(false);
                                let _ = write!(out, "ok\n");
                            }
                            _ => write_error(&mut out, "bad_args"),
                        }
                    }
                }
                "reply" => {
                    let Some(result) = parts.next() else {
                        write_error(&mut out, "bad_args");
                        return Some(out.len());
                    };
                    let Some(errno) = parts.next() else {
                        write_error(&mut out, "bad_args");
                        return Some(out.len());
                    };
                    if parts.next().is_some() {
                        write_error(&mut out, "extra_args");
                    } else {
                        match (parse_i64_token(result), parse_i32_token(errno)) {
                            (Some(result), Some(errno)) => {
                                if let Err(msg) = store_completion(result, errno) {
                                    write_error(&mut out, msg);
                                } else {
                                    let _ = write!(out, "ok\n");
                                }
                            }
                            _ => write_error(&mut out, "bad_args"),
                        }
                    }
                }
                "alloc_handle" => {
                    if parts.next().is_some() {
                        write_error(&mut out, "extra_args");
                    } else {
                        let handle = alloc_handle();
                        let _ = write!(out, "handle={}\n", handle);
                    }
                }
                "reset" => {
                    if parts.next().is_some() {
                        write_error(&mut out, "extra_args");
                    } else {
                        reset_state();
                        let _ = write!(out, "ok\n");
                    }
                }
                _ => write_error(&mut out, "bad_args"),
            }
            Some(out.len())
        }
        _ => None,
    }
}

fn read_insn<M: MemoryAccess>(mem: &mut M, addr: u64) -> Result<u32, M::Error> {
    let mut buf = [0u8; 4];
    mem.read(addr, &mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn decode_request<M: MemoryAccess>(
    mem: &mut M,
    regs: &Registers,
    insn_addr: u64,
) -> SemihostRequest {
    let op = regs.x0;
    let args_ptr = regs.x1;
    let kind = semihost_op(op);
    let pc_after = insn_addr.wrapping_add(4);
    let mut req = SemihostRequest {
        op,
        args_ptr,
        insn_addr,
        pc_after,
        kind,
        handle: 0,
        buf_ptr: 0,
        len: 0,
        mode: 0,
        decoded_ok: true,
    };

    match kind {
        Some(SemihostOp::Write0) => {
            req.buf_ptr = args_ptr;
        }
        Some(SemihostOp::Open) => match read_args_u64(mem, args_ptr, 3) {
            Ok(fields) => {
                req.buf_ptr = fields[0];
                req.mode = fields[1];
                req.len = fields[2];
            }
            Err(_) => req.decoded_ok = false,
        },
        Some(SemihostOp::Write) => match read_args_u64(mem, args_ptr, 3) {
            Ok(fields) => {
                req.handle = fields[0];
                req.buf_ptr = fields[1];
                req.len = fields[2];
            }
            Err(_) => req.decoded_ok = false,
        },
        Some(SemihostOp::Close) => match read_args_u64(mem, args_ptr, 1) {
            Ok(fields) => {
                req.handle = fields[0];
            }
            Err(_) => req.decoded_ok = false,
        },
        None => req.decoded_ok = false,
    }

    req
}

fn read_args_u64<M: MemoryAccess>(
    mem: &mut M,
    addr: u64,
    count: usize,
) -> Result<[u64; 3], M::Error> {
    let mut buf = [0u8; 24];
    let len = match count {
        1 => 8,
        3 => 24,
        _ => 0,
    };
    if len == 0 {
        return Ok([0u64; 3]);
    }
    mem.read(addr, &mut buf[..len])?;
    let mut fields = [0u64; 3];
    if count >= 1 {
        let mut tmp = [0u8; 8];
        tmp.copy_from_slice(&buf[0..8]);
        fields[0] = u64::from_le_bytes(tmp);
    }
    if count >= 2 {
        let mut tmp = [0u8; 8];
        tmp.copy_from_slice(&buf[8..16]);
        fields[1] = u64::from_le_bytes(tmp);
    }
    if count >= 3 {
        let mut tmp = [0u8; 8];
        tmp.copy_from_slice(&buf[16..24]);
        fields[2] = u64::from_le_bytes(tmp);
    }
    Ok(fields)
}

fn store_completion_locked(
    guard: &mut SemihostState,
    result: i64,
    errno: i32,
) -> Result<(), &'static str> {
    if !guard.pending {
        return Err("no_pending");
    }
    if guard.completion.is_some() {
        return Err("already_completed");
    }
    guard.completion = Some(SemihostCompletion { result, errno });
    Ok(())
}

fn store_completion(result: i64, errno: i32) -> Result<(), &'static str> {
    let mut guard = SEMIHOST.lock_irqsave();
    store_completion_locked(&mut guard, result, errno)
}

fn alloc_handle() -> u32 {
    let mut guard = SEMIHOST.lock_irqsave();
    guard.alloc_handle()
}

fn reset_state() {
    let mut guard = SEMIHOST.lock_irqsave();
    guard.clear();
}

fn write_semihost_query(out: &mut OutBuf<'_>) {
    let guard = SEMIHOST.lock_irqsave();
    if !guard.pending {
        let _ = write!(out, "no\n");
        return;
    }
    let Some(req) = guard.req else {
        let _ = write!(out, "no\n");
        return;
    };
    let op = (req.op & 0xffff_ffff) as u32;
    let _ = write!(
        out,
        "yes op=0x{:x} args=0x{:x} insn=0x{:x}\n",
        op, req.args_ptr, req.insn_addr
    );
}

fn write_semihost_info(out: &mut OutBuf<'_>) {
    let guard = SEMIHOST.lock_irqsave();
    if !guard.pending {
        write_error(out, "no_pending");
        return;
    }
    let Some(req) = guard.req else {
        write_error(out, "no_pending");
        return;
    };
    let Some(kind) = req.kind else {
        write_error(out, "unsupported_op");
        return;
    };
    if !req.decoded_ok && !matches!(kind, SemihostOp::Write0) {
        write_error(out, "decode_failed");
        return;
    }
    match kind {
        SemihostOp::Write0 => {
            let _ = write!(out, "op=write0 str=0x{:x}\n", req.buf_ptr);
        }
        SemihostOp::Open => {
            let _ = write!(
                out,
                "path=0x{:x} len={} mode={}\n",
                req.buf_ptr, req.len, req.mode
            );
        }
        SemihostOp::Write => {
            let _ = write!(
                out,
                "handle={} buf=0x{:x} len={}\n",
                req.handle, req.buf_ptr, req.len
            );
        }
        SemihostOp::Close => {
            let _ = write!(out, "handle={}\n", req.handle);
        }
    }
}

fn write_semihost_read<M: MemoryAccess>(
    out: &mut OutBuf<'_>,
    mem: &mut M,
    limit: usize,
) -> Result<(), &'static str> {
    if limit > SEMIHOST_READ_MAX {
        return Err("too_large");
    }
    let mut guard = SEMIHOST.lock_irqsave();
    if !guard.pending {
        return Err("no_pending");
    }
    let Some(req) = guard.req else {
        return Err("no_pending");
    };
    let Some(kind) = req.kind else {
        return Err("unsupported_op");
    };
    if !req.decoded_ok && !matches!(kind, SemihostOp::Write0) {
        return Err("decode_failed");
    }
    if guard.read_done {
        let _ = write!(out, "hex:\n");
        return Ok(());
    }

    let mut buf = [0u8; SEMIHOST_READ_MAX];
    let (actual_len, truncated) = match kind {
        SemihostOp::Write0 => {
            let addr = req
                .buf_ptr
                .checked_add(guard.read_offset)
                .ok_or("addr_overflow")?;
            let read_len = limit;
            if read_len == 0 {
                let _ = write!(out, "hex:\n");
                return Ok(());
            }
            mem.read(addr, &mut buf[..read_len])
                .map_err(|_| "mem_read")?;
            if let Some(pos) = buf[..read_len].iter().position(|&b| b == 0) {
                guard.read_done = true;
                guard.read_offset = guard.read_offset.saturating_add(pos as u64 + 1);
                (pos, false)
            } else {
                guard.read_offset = guard.read_offset.saturating_add(read_len as u64);
                (read_len, true)
            }
        }
        SemihostOp::Open | SemihostOp::Write => {
            let total_len = req.len;
            let offset = guard.read_offset;
            if offset >= total_len {
                guard.read_done = true;
                let _ = write!(out, "hex:\n");
                return Ok(());
            }
            let remaining = total_len - offset;
            let read_len = core::cmp::min(remaining, limit as u64) as usize;
            let addr = req.buf_ptr.checked_add(offset).ok_or("addr_overflow")?;
            mem.read(addr, &mut buf[..read_len])
                .map_err(|_| "mem_read")?;
            guard.read_offset = offset + read_len as u64;
            (read_len, guard.read_offset < total_len)
        }
        SemihostOp::Close => return Err("bad_op"),
    };

    let _ = out.try_write_str("hex:");
    let _ = push_hex_bytes(out, &buf[..actual_len]);
    let _ = out.try_write_str(if truncated { " truncated=1\n" } else { "\n" });
    Ok(())
}

fn write_bytes(out: &mut OutBuf<'_>, bytes: &[u8]) -> Result<(), &'static str> {
    out.try_write_bytes(bytes).map_err(|_| "buf_overflow")
}

fn write_hex_u64(out: &mut OutBuf<'_>, mut val: u64) -> Result<(), &'static str> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut tmp = [0u8; 16];
    let mut len = 0usize;

    if val == 0 {
        tmp[0] = b'0';
        len = 1;
    } else {
        while val != 0 {
            tmp[len] = HEX[(val & 0xf) as usize];
            len += 1;
            val >>= 4;
        }
    }

    let mut out_buf = [0u8; 16];
    for i in 0..len {
        out_buf[i] = tmp[len - 1 - i];
    }
    write_bytes(out, &out_buf[..len])
}

fn cstring_len<M: MemoryAccess>(
    mem: &mut M,
    addr: u64,
    cap: usize,
) -> Result<Option<usize>, &'static str> {
    let mut offset = 0usize;
    let mut buf = [0u8; 64];
    while offset < cap {
        let read_len = core::cmp::min(cap - offset, buf.len());
        let read_addr = addr.checked_add(offset as u64).ok_or("addr_overflow")?;
        mem.read(read_addr, &mut buf[..read_len])
            .map_err(|_| "mem_read")?;
        if let Some(pos) = buf[..read_len].iter().position(|&b| b == 0) {
            return Ok(Some(offset + pos));
        }
        offset += read_len;
    }
    Ok(None)
}

fn parse_usize_token(token: &str) -> Option<usize> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        usize::from_str_radix(hex, 16).ok()
    } else {
        usize::from_str_radix(token, 10).ok()
    }
}

fn parse_i64_token(token: &str) -> Option<i64> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let (neg, rest) = if let Some(rest) = token.strip_prefix('-') {
        (true, rest)
    } else {
        (false, token)
    };
    if rest.is_empty() {
        return None;
    }
    let value = if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()?
    } else {
        u64::from_str_radix(rest, 10).ok()?
    };
    let value = value as i128;
    let value = if neg { -value } else { value };
    if value < i64::MIN as i128 || value > i64::MAX as i128 {
        None
    } else {
        Some(value as i64)
    }
}

fn parse_i32_token(token: &str) -> Option<i32> {
    let value = parse_i64_token(token)?;
    if value < i32::MIN as i64 || value > i32::MAX as i64 {
        None
    } else {
        Some(value as i32)
    }
}

fn write_error(out: &mut OutBuf<'_>, msg: &str) {
    let _ = write!(out, "error {}\n", msg);
}

fn push_hex_bytes(out: &mut OutBuf<'_>, bytes: &[u8]) -> fmt::Result {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 2];
    for &b in bytes {
        buf[0] = HEX[(b >> 4) as usize];
        buf[1] = HEX[(b & 0xf) as usize];
        out.try_write_bytes(&buf)?;
    }
    Ok(())
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
        if self.len >= self.buf.len() {
            return Err(fmt::Error);
        }
        let cap = self.buf.len() - self.len;
        let copy_len = core::cmp::min(cap, bytes.len());
        self.buf[self.len..self.len + copy_len].copy_from_slice(&bytes[..copy_len]);
        self.len += copy_len;
        if copy_len < bytes.len() {
            return Err(fmt::Error);
        }
        Ok(())
    }

    fn try_write_str(&mut self, s: &str) -> fmt::Result {
        self.try_write_bytes(s.as_bytes())
    }
}

impl fmt::Write for OutBuf<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.try_write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyMem;

    impl MemoryAccess for DummyMem {
        type Error = ();

        fn read(&mut self, _addr: u64, _dst: &mut [u8]) -> Result<(), Self::Error> {
            Err(())
        }

        fn write(&mut self, _addr: u64, _src: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn reset_for_test() {
        let mut guard = SEMIHOST.lock_irqsave();
        guard.clear();
        guard.fileio_enabled = true;
    }

    fn test_request(op: u32, buf_ptr: u64) -> SemihostRequest {
        SemihostRequest {
            op: u64::from(op),
            args_ptr: 0,
            insn_addr: 0,
            pc_after: 0,
            kind: semihost_op(u64::from(op)),
            handle: 0,
            buf_ptr,
            len: 0,
            mode: 0,
            decoded_ok: true,
        }
    }

    struct BufMem {
        base: u64,
        data: [u8; 128],
    }

    impl BufMem {
        fn new(base: u64) -> Self {
            Self {
                base,
                data: [0u8; 128],
            }
        }

        fn write_at(&mut self, offset: usize, src: &[u8]) {
            let end = offset.saturating_add(src.len());
            self.data[offset..end].copy_from_slice(src);
        }
    }

    impl MemoryAccess for BufMem {
        type Error = ();

        fn read(&mut self, addr: u64, dst: &mut [u8]) -> Result<(), Self::Error> {
            let start = addr.checked_sub(self.base).ok_or(())? as usize;
            let end = start.checked_add(dst.len()).ok_or(())?;
            if end > self.data.len() {
                return Err(());
            }
            dst.copy_from_slice(&self.data[start..end]);
            Ok(())
        }

        fn write(&mut self, addr: u64, src: &[u8]) -> Result<(), Self::Error> {
            let start = addr.checked_sub(self.base).ok_or(())? as usize;
            let end = start.checked_add(src.len()).ok_or(())?;
            if end > self.data.len() {
                return Err(());
            }
            self.data[start..end].copy_from_slice(src);
            Ok(())
        }
    }

    #[cfg_attr(target_arch = "aarch64", test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn hlt_decode_matches_semihost() {
        assert!(is_semihost_hlt(0xD45E_0000));
        assert!(!is_semihost_hlt(0xD440_0000));
        assert!(!is_semihost_hlt(0xD420_0000));
    }

    #[cfg_attr(target_arch = "aarch64", test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn resume_gate_transitions() {
        reset_for_test();
        let req = SemihostRequest {
            insn_addr: 0x1000,
            pc_after: 0x1004,
            ..test_request(SYS_WRITE0, 0x2000)
        };
        SEMIHOST.lock_irqsave().start_request(req);
        assert!(matches!(resume_gate(), ResumeGate::Hold));
        {
            let mut guard = SEMIHOST.lock_irqsave();
            guard.completion = Some(SemihostCompletion {
                result: 0,
                errno: 0,
            });
        }
        let gate = resume_gate();
        match gate {
            ResumeGate::Proceed(Some((got_req, _))) => {
                assert_eq!(got_req.insn_addr, req.insn_addr);
            }
            _ => panic!("expected completion"),
        }
        let gate = resume_gate();
        match gate {
            ResumeGate::Proceed(None) => {}
            _ => panic!("expected no pending state"),
        }
    }

    #[cfg_attr(target_arch = "aarch64", test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn monitor_query_has_newline() {
        reset_for_test();
        let mut out = [0u8; 64];
        let mut mem = DummyMem;
        let len = monitor_command("hp semihost?", &mut out, &mut mem).unwrap();
        assert_eq!(&out[..len], b"no\n");
    }

    #[cfg_attr(target_arch = "aarch64", test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn monitor_reply_requires_pending() {
        reset_for_test();
        let mut out = [0u8; 64];
        let mut mem = DummyMem;
        let len = monitor_command("hp semihost reply 0 0", &mut out, &mut mem).unwrap();
        assert!(
            core::str::from_utf8(&out[..len])
                .unwrap_or("")
                .starts_with("error")
        );
    }

    #[cfg_attr(target_arch = "aarch64", test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn monitor_read_advances_write0_chunks() {
        reset_for_test();
        let mut mem = BufMem::new(0x1000);
        mem.write_at(0, b"abcd\0");
        let req = test_request(SYS_WRITE0, 0x1000);
        SEMIHOST.lock_irqsave().start_request(req);

        for expected in [
            b"hex:616263 truncated=1\n".as_slice(),
            b"hex:64\n",
            b"hex:\n",
        ] {
            let mut out = [0u8; 64];
            let len = monitor_command("hp semihost read 3", &mut out, &mut mem).unwrap();
            assert_eq!(&out[..len], expected);
        }
    }

    #[cfg_attr(target_arch = "aarch64", test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn monitor_read_advances_bounded_chunks() {
        for op in [SYS_OPEN, SYS_WRITE] {
            reset_for_test();
            let mut mem = BufMem::new(0x1000);
            mem.write_at(0, b"abcd");
            let req = SemihostRequest {
                len: 4,
                ..test_request(op, 0x1000)
            };
            SEMIHOST.lock_irqsave().start_request(req);

            for expected in [
                b"hex:616263 truncated=1\n".as_slice(),
                b"hex:64\n",
                b"hex:\n",
            ] {
                let mut out = [0u8; 64];
                let len = monitor_command("hp semihost read 3", &mut out, &mut mem).unwrap();
                assert_eq!(&out[..len], expected);
            }
        }
    }

    #[cfg_attr(target_arch = "aarch64", test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn fileio_write0_builds_fwrite_with_len() {
        reset_for_test();
        let mut mem = BufMem::new(0x1000);
        mem.write_at(0, b"hi\0");

        let req = test_request(SYS_WRITE0, 0x1000);
        SEMIHOST.lock_irqsave().start_request(req);

        let mut out = [0u8; 64];
        let len = fileio_try_build_request(&mut mem, &mut out)
            .unwrap()
            .expect("expected request");
        assert_eq!(&out[..len], b"Fwrite,1,1000/2");
    }

    #[cfg_attr(target_arch = "aarch64", test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn fileio_open_builds_and_rejects_unknown_mode() {
        reset_for_test();
        let mut mem = DummyMem;
        let req = SemihostRequest {
            len: 0x10,
            mode: 4,
            ..test_request(SYS_OPEN, 0x2000)
        };
        SEMIHOST.lock_irqsave().start_request(req);

        let mut out = [0u8; 64];
        let len = fileio_try_build_request(&mut mem, &mut out)
            .unwrap()
            .expect("expected request");
        assert_eq!(&out[..len], b"Fopen,2000/10,241,1b6");

        reset_for_test();
        let req = SemihostRequest { mode: 7, ..req };
        SEMIHOST.lock_irqsave().start_request(req);
        let mut out = [0u8; 64];
        let built = fileio_try_build_request(&mut mem, &mut out).unwrap();
        assert!(built.is_none());
        let guard = SEMIHOST.lock_irqsave();
        let completion = guard.completion.expect("missing completion");
        assert_eq!(completion.result, -1);
        assert_eq!(completion.errno, EINVAL);
    }

    #[cfg_attr(target_arch = "aarch64", test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn fileio_write_reply_maps_bytes_not_written() {
        reset_for_test();
        let mut mem = DummyMem;
        let req = SemihostRequest {
            handle: 3,
            len: 10,
            ..test_request(SYS_WRITE, 0x3000)
        };
        SEMIHOST.lock_irqsave().start_request(req);
        let mut out = [0u8; 64];
        let _ = fileio_try_build_request(&mut mem, &mut out).unwrap();
        fileio_on_reply(7, 0).expect("fileio reply failed");

        let guard = SEMIHOST.lock_irqsave();
        let completion = guard.completion.expect("missing completion");
        assert_eq!(completion.result, 3);
    }
}
