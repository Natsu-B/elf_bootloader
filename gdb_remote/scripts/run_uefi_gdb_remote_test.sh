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
GDB_BIN=${GDB_BIN:-$(command -v gdb-multiarch || command -v gdb)}
UART_PORT=${UART_PORT:-12355}
UART_TRANSPORT=${UART_TRANSPORT:-auto}

QEMU_LOG="$SCRIPT_DIR/../bin/qemu_uefi_gdb_remote_${UART_PORT}.log"
UART_PIPE_BASE=""
UART_SOCKET_PATH=""
QEMU_STDIO_IN=""
QEMU_STDIO_OUT=""

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
      -serial null \
      -serial "$serial_arg" \
      -drive "file=fat:rw:$SCRIPT_DIR/../bin,format=raw,if=none,media=disk,id=disk" \
      -device virtio-blk-device,drive=disk,bus=virtio-mmio-bus.0

    if [ -n "$SOCK_TRIMMED" ]; then
        rm -f "$SOCK_TRIMMED"
        set -- "$@" -gdb "unix:${SOCK_TRIMMED},server,nowait"
    fi

    if [ -n "${QEMU_STDIO_IN:-}" ] || [ -n "${QEMU_STDIO_OUT:-}" ]; then
        "$@" <"$QEMU_STDIO_IN" >"$QEMU_STDIO_OUT" 2>"$QEMU_LOG" &
    else
        "$@" 2>"$QEMU_LOG" &
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

# Give firmware time to finish early init (UART0 console is routed to null anyway).
sleep 3

if [ "$USE_PIPE_CLIENT" -eq 1 ]; then
    if ! command -v python3 >/dev/null 2>&1; then
        echo "UART_TRANSPORT=pipe requires python3 for the RSP client." >&2
        exit 1
    fi

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
while True:
    try:
        in_fd = os.open(pipe_write, os.O_WRONLY | os.O_NONBLOCK)
        break
    except OSError as exc:
        if exc.errno == errno.ENXIO:
            time.sleep(0.05)
            continue
        raise
log("RSP pipes connected")

def read_byte(timeout):
    deadline = time.time() + timeout
    while True:
        remaining = deadline - time.time()
        if remaining <= 0:
            raise TimeoutError("timeout waiting for byte")
        rlist, _, _ = select.select([out_fd], [], [], remaining)
        if not rlist:
            continue
        data = os.read(out_fd, 1)
        if data:
            return data

def send_packet(payload):
    checksum = sum(payload) & 0xFF
    packet = b"$" + payload + b"#" + f"{checksum:02x}".encode()
    os.write(in_fd, packet)

def recv_ack(timeout):
    while True:
        b = read_byte(timeout)
        if b in (b"+", b"-", b"$"):
            return b

def recv_packet(timeout, first_byte=None):
    if first_byte is None:
        while True:
            b = read_byte(timeout)
            if b == b"$":
                break
    payload = bytearray()
    while True:
        b = read_byte(timeout)
        if b == b"#":
            break
        payload += b
    checksum = read_byte(timeout) + read_byte(timeout)
    calc = sum(payload) & 0xFF
    if checksum.lower() != f"{calc:02x}".encode():
        os.write(in_fd, b"-")
        raise ValueError("bad checksum")
    os.write(in_fd, b"+")
    return bytes(payload)

def roundtrip(payload_str, timeout=30):
    payload = payload_str.encode()
    log(f"send {payload_str}")
    while True:
        send_packet(payload)
        ack = recv_ack(timeout)
        if ack == b"-":
            continue
        if ack == b"$":
            resp = recv_packet(timeout, first_byte=ack)
            log(f"recv {resp!r}")
            return resp
        resp = recv_packet(timeout)
        log(f"recv {resp!r}")
        return resp

def hex_encode(text):
    return "".join(f"{b:02x}" for b in text.encode())

try:
    roundtrip("qSupported")
    roundtrip("qRcmd," + hex_encode("exit 0"))
    roundtrip("vKill")
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
set remotetimeout 20
set debug remote 1
${GDB_TARGET_LINE}
monitor exit 0
quit 0
EOF

"$GDB_BIN" --batch -x "$TMP_GDB_SCRIPT"
STATUS=$?

rm -f "$TMP_GDB_SCRIPT"

exit $STATUS
