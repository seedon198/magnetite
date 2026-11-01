#!/usr/bin/env bash
# Boot the kernel ISO headless in QEMU and assert the whole boot succeeds: the
# serial console shows the root task's success line and `ready:`, which `kmain`
# logs only after every boot-time self-check has passed. Fails at once if the
# kernel panics. Exits 0 on success.
#
# The markers match whole kernel log lines from their start, so text a user
# task sends to the console (logged as `[INFO] console[N]: ...`) can never
# satisfy them.
#
# We poll the serial log and stop as soon as either appears (no fixed long
# wait). Pure serial-grep: no special kernel instrumentation is needed.
#
# Usage: scripts/boot-test.sh <bios|uefi>
set -euo pipefail

cd "$(dirname "$0")/.."

MODE="${1:-bios}"
MARKERS=(
    '^\[INFO\] task: root task confined'
    '^\[INFO\] ready: '
)
PANIC_MARKER="KERNEL PANIC"
ISO="build/kernel.iso"
LOG="build/serial-${MODE}.log"
TIMEOUT_SECS="${BOOT_TEST_TIMEOUT:-60}"

if [[ ! -f "$ISO" ]]; then
    echo "boot-test: no ISO at $ISO (run 'cargo xtask iso' first)" >&2
    exit 1
fi

qemu_args=(-M q35 -cpu qemu64,+smep,+smap -m 256M -cdrom "$ISO" -serial "file:$LOG" -display none -no-reboot)

case "$MODE" in
    bios)
        qemu_args+=(-boot d)
        ;;
    uefi)
        ovmf=""
        for cand in \
            /usr/share/OVMF/OVMF_CODE_4M.fd \
            /usr/share/OVMF/OVMF_CODE.fd \
            /usr/share/qemu/edk2-x86_64-code.fd \
            /usr/share/edk2/x64/OVMF_CODE.fd \
            /usr/share/edk2-ovmf/x64/OVMF_CODE.fd \
            /opt/homebrew/opt/qemu/share/qemu/edk2-x86_64-code.fd \
            /opt/homebrew/share/qemu/edk2-x86_64-code.fd \
            "${OVMF:-}"; do
            if [[ -n "$cand" && -f "$cand" ]]; then
                ovmf="$cand"
                break
            fi
        done
        if [[ -z "$ovmf" ]]; then
            echo "boot-test: OVMF firmware not found (set OVMF=/path/to/edk2-x86_64-code.fd)" >&2
            exit 1
        fi
        qemu_args+=(-drive "if=pflash,unit=0,format=raw,file=${ovmf},readonly=on")
        ;;
    *)
        echo "usage: boot-test.sh <bios|uefi>" >&2
        exit 2
        ;;
esac

: > "$LOG"
echo "boot-test[$MODE]: booting $ISO ..."
qemu-system-x86_64 "${qemu_args[@]}" &
qpid=$!

cleanup() { kill "$qpid" 2>/dev/null || true; wait "$qpid" 2>/dev/null || true; }
trap cleanup EXIT

# Poll twice a second for the marker, or until QEMU exits or we time out.
for ((i = 0; i < TIMEOUT_SECS * 2; i++)); do
    # Panic first: a panic and the markers can land in the same poll.
    if grep -qF "$PANIC_MARKER" "$LOG" 2>/dev/null; then
        sleep 1 # let the panic report finish
        echo "boot-test[$MODE]: FAIL - the kernel panicked. Serial log:" >&2
        echo "---------------------------------------------------------------" >&2
        cat "$LOG" >&2 || true
        echo "---------------------------------------------------------------" >&2
        exit 1
    fi
    found=1
    for m in "${MARKERS[@]}"; do
        grep -qE "$m" "$LOG" 2>/dev/null || found=0
    done
    if [[ "$found" -eq 1 ]]; then
        echo "boot-test[$MODE]: PASS - the root task passed and the kernel is ready"
        exit 0
    fi
    if ! kill -0 "$qpid" 2>/dev/null; then
        echo "boot-test[$MODE]: QEMU exited before the markers appeared" >&2
        break
    fi
    sleep 0.5
done

echo "boot-test[$MODE]: FAIL - success markers not seen within ${TIMEOUT_SECS}s. Serial log:" >&2
echo "---------------------------------------------------------------" >&2
cat "$LOG" >&2 || true
echo "---------------------------------------------------------------" >&2
exit 1
