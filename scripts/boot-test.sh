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
# A healthy boot also logs no warning or error: any `[WARN]` or `[ERROR]` line
# the mode does not expect fails the test (#71). Every in-kernel self-check
# panics on failure, and the fallbacks that only warn (a missing boot module,
# a timer that cannot be calibrated) are caught here.
#
# We poll the serial log and stop as soon as either appears (no fixed long
# wait). Pure serial-grep: no special kernel instrumentation is needed.
#
# `uefi-nopit` boots UEFI on a machine with no 8254 PIT. LAPIC timer
# calibration must notice and fall back to cooperative scheduling (#109), so
# that mode also requires the warning and a cooperative `ready:` line.
#
# Usage: scripts/boot-test.sh <bios|uefi|uefi-nopit>
set -euo pipefail

cd "$(dirname "$0")/.."

MODE="${1:-bios}"
MARKERS=(
    '^\[INFO\] task: root task confined'
    '^\[INFO\] ready: '
)
PANIC_MARKER="KERNEL PANIC"
# Warnings this mode expects, as one extended regex ('' = none).
EXPECTED_WARNINGS='^$'
ISO="build/kernel.iso"
LOG="build/serial-${MODE}.log"
TIMEOUT_SECS="${BOOT_TEST_TIMEOUT:-60}"

if [[ ! -f "$ISO" ]]; then
    echo "boot-test: no ISO at $ISO (run 'cargo xtask iso' first)" >&2
    exit 1
fi

MACHINE=q35
if [[ "$MODE" == uefi-nopit ]]; then
    MACHINE=q35,pit=off
    MARKERS+=(
        '^\[WARN\] lapic: timer start failed: CalibrationFailed'
        '^\[INFO\] ready: .*cooperative scheduling'
    )
    EXPECTED_WARNINGS='^\[WARN\] lapic: timer start failed: CalibrationFailed$'
fi

qemu_args=(-M "$MACHINE" -cpu qemu64,+smep,+smap -m 256M -cdrom "$ISO" -serial "file:$LOG" -display none -no-reboot)

case "$MODE" in
    bios)
        qemu_args+=(-boot d)
        ;;
    uefi | uefi-nopit)
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
        echo "usage: boot-test.sh <bios|uefi|uefi-nopit>" >&2
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
        unexpected="$(tr -d '\r' < "$LOG" | grep -E '^\[(WARN|ERROR)\]' | grep -vE "$EXPECTED_WARNINGS" || true)"
        if [[ -n "$unexpected" ]]; then
            echo "boot-test[$MODE]: FAIL - the boot logged unexpected warnings:" >&2
            echo "$unexpected" >&2
            exit 1
        fi
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
