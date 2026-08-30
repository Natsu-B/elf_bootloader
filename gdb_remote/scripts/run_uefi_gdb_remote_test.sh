#!/bin/sh
set -eu
export LC_ALL=C

PATH_TO_ELF="${1:-}"

if [ -z "$PATH_TO_ELF" ]; then
    echo "usage: $0 <path-to-uefi-test-elf>"
    exit 1
fi

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
BIN_DIR="$SCRIPT_DIR/../bin/EFI/BOOT"

rm -rf "$SCRIPT_DIR/../bin/EFI"
mkdir -p "$BIN_DIR"
cp "$PATH_TO_ELF" "$BIN_DIR/BOOTAA64.EFI"

FIRMWARE="$REPO_ROOT/test/RELEASEAARCH64_QEMU_EFI.fd"
if [ ! -f "$FIRMWARE" ]; then
    echo "Missing UEFI firmware: $FIRMWARE" >&2
    exit 1
fi

QEMU_BIN=${QEMU_BIN:-qemu-system-aarch64}
GDB_BIN=${GDB_BIN:-gdb}
UART_PORT=${UART_PORT:-12355}
UART_TRANSPORT=${UART_TRANSPORT:-auto}

QEMU_LOG="$SCRIPT_DIR/../bin/qemu_uefi_gdb_remote_${UART_PORT}.log"
UART_PIPE_BASE=""
UART_SOCKET_PATH=""
QEMU_STDIO_IN=""
QEMU_STDIO_OUT=""
PIPE_HOLD_FDS=0
TMP_GDB_SCRIPT=""

# If xtask provided a QEMU gdbstub socket path, enable it for timeout debugging.
# Trim leading/trailing whitespace to avoid creating a socket named " " (space).
SOCK_RAW="${XTASK_QEMU_GDB_SOCKET:-}"
SOCK_TRIMMED="$(printf '%s' "$SOCK_RAW" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"

QEMU_PID=""
cleanup() {
    if [ -n "${QEMU_PID:-}" ]; then
        kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
    fi
    if [ -n "${UART_PIPE_BASE:-}" ]; then
        rm -f "${UART_PIPE_BASE}.in" "${UART_PIPE_BASE}.out"
    fi
    if [ -n "${UART_SOCKET_PATH:-}" ]; then
        rm -f "${UART_SOCKET_PATH}"
    fi
    if [ -n "${SOCK_TRIMMED:-}" ]; then
        rm -f "${SOCK_TRIMMED}"
    fi
    if [ -n "${TMP_GDB_SCRIPT:-}" ]; then
        rm -f "${TMP_GDB_SCRIPT}"
    fi
    if [ "${PIPE_HOLD_FDS:-0}" -eq 1 ]; then
        exec 3>&-
        exec 4>&-
    fi
}
trap cleanup EXIT

start_qemu() {
    serial_arg="$1"
    set -- "$QEMU_BIN" \
      -M virt,gic-version=3,secure=off,virtualization=on \
      -global virtio-mmio.force-legacy=off \
      -cpu cortex-a53 -smp 4 -m 1G \
      -bios "$FIRMWARE" \
      -display none \
      -monitor none \
      -semihosting-config enable=on,target=native \
      -no-reboot -no-shutdown \
      -serial "$serial_arg" \
      -serial null \
      -drive "file=fat:rw:$SCRIPT_DIR/../bin,format=raw,if=none,media=disk,id=disk" \
      -device virtio-blk-device,drive=disk,bus=virtio-mmio-bus.0

    if [ -n "$SOCK_TRIMMED" ]; then
        rm -f "$SOCK_TRIMMED"
        set -- "$@" -gdb "unix:${SOCK_TRIMMED},server,nowait"
    fi

    if [ -n "${QEMU_STDIO_IN:-}" ] || [ -n "${QEMU_STDIO_OUT:-}" ]; then
        "$@" <"$QEMU_STDIO_IN" >"$QEMU_STDIO_OUT" 2>"$QEMU_LOG" &
    else
        "$@" >"$QEMU_LOG" 2>&1 &
    fi
    QEMU_PID=$!
}

fail_with_log() {
    echo "QEMU failed to start. Log:" >&2
    sed -n '1,200p' "$QEMU_LOG" >&2
    exit 1
}

case "$UART_TRANSPORT" in
    auto)
        UART_MODES="tcp unix pty pipe"
        ;;
    tcp)
        UART_MODES="tcp"
        ;;
    unix)
        UART_MODES="unix"
        ;;
    pty)
        UART_MODES="pty"
        ;;
    pipe)
        UART_MODES="pipe"
        ;;
    *)
        echo "Unknown UART_TRANSPORT: $UART_TRANSPORT (expected auto|tcp|unix|pty|pipe)" >&2
        exit 1
        ;;
esac

UART_MODE=""
for mode in $UART_MODES; do
    rm -f "$QEMU_LOG"
    QEMU_STDIO_IN=""
    QEMU_STDIO_OUT=""
    if [ "$mode" = "tcp" ]; then
        start_qemu "tcp:127.0.0.1:${UART_PORT},server,nowait"
    elif [ "$mode" = "unix" ]; then
        if ! command -v nc >/dev/null 2>&1; then
            if [ "$UART_TRANSPORT" = "unix" ]; then
                echo "UART_TRANSPORT=unix requires 'nc' with UNIX socket support." >&2
                exit 1
            fi
            continue
        fi
        UART_SOCKET_PATH="${SCRIPT_DIR}/../bin/qemu_uart_${UART_PORT}_$$.sock"
        rm -f "${UART_SOCKET_PATH}"
        start_qemu "unix:${UART_SOCKET_PATH},server,nowait"
    elif [ "$mode" = "pty" ]; then
        start_qemu "pty"
    else
        UART_PIPE_BASE="${SCRIPT_DIR}/../bin/qemu_uart_${UART_PORT}_$$"
        QEMU_STDIO_IN="${UART_PIPE_BASE}.in"
        QEMU_STDIO_OUT="${UART_PIPE_BASE}.out"
        rm -f "${QEMU_STDIO_IN}" "${QEMU_STDIO_OUT}"
        mkfifo "${QEMU_STDIO_IN}" "${QEMU_STDIO_OUT}"
        # Keep both FIFOs open until the client starts so QEMU can reach READY.
        exec 3<>"${QEMU_STDIO_IN}"
        exec 4<>"${QEMU_STDIO_OUT}"
        PIPE_HOLD_FDS=1
        start_qemu "stdio"
    fi

    # Fail fast if QEMU died immediately (prevents long GDB connect timeouts).
    sleep 1
    if kill -0 "$QEMU_PID" 2>/dev/null; then
        UART_MODE="$mode"
        break
    fi

    if [ "$mode" = "tcp" ] && grep -q "Failed to create a socket: Operation not permitted" "$QEMU_LOG"; then
        wait "$QEMU_PID" 2>/dev/null || true
        continue
    fi
    if [ "$mode" = "unix" ] && { grep -q "Permission denied" "$QEMU_LOG" || grep -q "Operation not permitted" "$QEMU_LOG"; }; then
        wait "$QEMU_PID" 2>/dev/null || true
        continue
    fi
    if [ "$mode" = "pty" ] && grep -q "Failed to create PTY" "$QEMU_LOG"; then
        wait "$QEMU_PID" 2>/dev/null || true
        continue
    fi

    fail_with_log
done

if [ -z "$UART_MODE" ]; then
    fail_with_log
fi

GDB_TARGET_LINE=""
USE_PIPE_CLIENT=0
if [ "$UART_MODE" = "pty" ]; then
    for _ in 1 2 3 4 5; do
        UART_TARGET=$(sed -n 's/.*char device redirected to \([^ ]*\) .*/\1/p' "$QEMU_LOG" | tail -n 1)
        if [ -n "$UART_TARGET" ]; then
            break
        fi
        sleep 1
    done

    if [ -z "$UART_TARGET" ]; then
        echo "Failed to detect QEMU UART PTY. Log:" >&2
        sed -n '1,200p' "$QEMU_LOG" >&2
        exit 1
    fi

    GDB_TARGET_LINE="target remote ${UART_TARGET}"
elif [ "$UART_MODE" = "unix" ]; then
    GDB_TARGET_LINE="target remote | nc -U '${UART_SOCKET_PATH}'"
elif [ "$UART_MODE" = "pipe" ]; then
    PIPE_IN="${UART_PIPE_BASE}.in"
    PIPE_OUT="${UART_PIPE_BASE}.out"
    USE_PIPE_CLIENT=1
else
    GDB_TARGET_LINE="target remote 127.0.0.1:${UART_PORT}"
fi

# UART0 is also the firmware console. Attach only after the EFI test has initialized
# the RSP server, so no firmware console bytes can enter GDB's remote stream.
ready_waited=0
while ! grep -Fq "GDB_REMOTE_READY" "$QEMU_LOG" 2>/dev/null; do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        echo "QEMU exited before the UEFI GDB server became ready. Log:" >&2
        sed -n '1,200p' "$QEMU_LOG" >&2
        exit 1
    fi
    if [ "$ready_waited" -ge 30 ]; then
        echo "Timed out waiting 30s for the UEFI GDB server readiness marker. Log:" >&2
        sed -n '1,200p' "$QEMU_LOG" >&2
        exit 124
    fi
    sleep 1
    ready_waited=$((ready_waited + 1))
done

if [ "$USE_PIPE_CLIENT" -eq 1 ]; then
    if ! command -v python3 >/dev/null 2>&1; then
        echo "UART_TRANSPORT=pipe requires python3 for the RSP client." >&2
        exit 1
    fi

    exec 3>&-
    exec 4>&-
    PIPE_HOLD_FDS=0

    python3 -u - "$PIPE_IN" "$PIPE_OUT" <<'PY'
import errno
import os
import select
import sys
import time

pipe_write = sys.argv[1]
pipe_read = sys.argv[2]
debug = os.getenv("RSP_DEBUG") == "1"

def log(msg):
    if debug:
        sys.stderr.write(msg + "\n")

log(f"RSP client using write={pipe_write} read={pipe_read}")
out_fd = os.open(pipe_read, os.O_RDONLY | os.O_NONBLOCK)
connect_deadline = time.monotonic() + 10
while True:
    try:
        in_fd = os.open(pipe_write, os.O_WRONLY | os.O_NONBLOCK)
        break
    except OSError as exc:
        if exc.errno == errno.ENXIO:
            if time.monotonic() >= connect_deadline:
                raise TimeoutError("timeout opening UART input pipe")
            time.sleep(0.05)
            continue
        raise
log("RSP pipes connected")

def read_byte(deadline):
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("timeout waiting for byte")
        rlist, _, _ = select.select([out_fd], [], [], remaining)
        if not rlist:
            continue
        data = os.read(out_fd, 1)
        if data:
            return data
        raise EOFError("UART output pipe closed")

def send_packet(payload):
    checksum = sum(payload) & 0xFF
    packet = b"$" + payload + b"#" + f"{checksum:02x}".encode()
    os.write(in_fd, packet)

def recv_ack(deadline):
    while True:
        b = read_byte(deadline)
        if b in (b"+", b"-", b"$"):
            return b

def recv_packet(deadline, first_byte=None):
    if first_byte is None:
        while True:
            b = read_byte(deadline)
            if b == b"$":
                break
    payload = bytearray()
    while True:
        b = read_byte(deadline)
        if b == b"#":
            break
        payload += b
    checksum = read_byte(deadline) + read_byte(deadline)
    calc = sum(payload) & 0xFF
    if checksum.lower() != f"{calc:02x}".encode():
        os.write(in_fd, b"-")
        raise ValueError("bad checksum")
    os.write(in_fd, b"+")
    return bytes(payload)

def roundtrip(payload_str, expected=None, timeout=30):
    payload = payload_str.encode()
    deadline = time.monotonic() + timeout
    log(f"send {payload_str}")
    send_packet(payload)
    while True:
        ack = recv_ack(deadline)
        if ack == b"-":
            send_packet(payload)
            continue
        if ack == b"$":
            resp = recv_packet(deadline, first_byte=ack)
        else:
            resp = recv_packet(deadline)
        log(f"recv {resp!r}")
        if expected is None or resp == expected:
            return resp
        log(f"ignore unexpected response {resp!r}")

def hex_encode(text):
    return "".join(f"{b:02x}" for b in text.encode())

try:
    supported = roundtrip("qSupported")
    if not supported.startswith(b"PacketSize="):
        raise ValueError(f"unexpected qSupported response: {supported!r}")
    roundtrip("qRcmd," + hex_encode("exit 0"), expected=b"OK")
    roundtrip("vKill", expected=b"OK")
except Exception as exc:
    sys.stderr.write(f"RSP client failed: {exc}\n")
    sys.exit(1)
PY
    exit $?
fi

# Generate a temporary GDB script bound to the chosen target.
TMP_GDB_SCRIPT=$(mktemp "${SCRIPT_DIR}/../bin/gdb_remote_test.XXXXXX.gdb")
cat > "$TMP_GDB_SCRIPT" <<EOF
set architecture aarch64
set confirm off
set pagination off
set remotetimeout 5
set debug remote 1
${GDB_TARGET_LINE}
monitor exit 0
quit 0
EOF

if "$GDB_BIN" --batch -x "$TMP_GDB_SCRIPT"; then
    STATUS=0
else
    STATUS=$?
fi

exit $STATUS
