#!/usr/bin/env bash
# Check that every `global_asm!` entry point in the kernel is a text symbol
# (#138). A `global_asm!` block that switches section and does not switch back
# puts whatever asm the compiler emits after it, in the same codegen unit, into
# that section: in #137 the syscall and ring-3 entry stubs landed in
# non-executable .rodata on CI's build only. Which asm follows which depends on
# codegen-unit partitioning, so the kernel is checked twice: as built normally,
# and with one codegen unit, where all of hal's module asm shares one unit.
#
# Exit non-zero on any violation. Portable bash (macOS and Linux).
set -euo pipefail

cd "$(dirname "$0")/.."

# Entry points defined in global_asm! (hal), which must be code. VECTOR_STUBS
# is a data table and lives in .rodata on purpose.
SYMBOLS=(
    syscall_entry syscall_compat_entry enter_user_once_asm
    context_switch thread_start probe_read_byte probe_read_fixup
    common_stub vector_stub_0 vector_stub_14 vector_stub_255
)

sysroot="$(rustc --print sysroot)"
nm="$(find "$sysroot/lib/rustlib" -name llvm-nm -type f 2>/dev/null | head -n 1)"
if [[ -z "$nm" ]]; then
    echo "check-asm-sections: llvm-nm not found (rustup component add llvm-tools)" >&2
    exit 1
fi

fail=0
check() {
    local label="$1" elf="$2" sym kind
    for sym in "${SYMBOLS[@]}"; do
        kind="$("$nm" "$elf" | awk -v s="$sym" '$3 == s { print $2; exit }')"
        if [[ "$kind" != "T" && "$kind" != "t" ]]; then
            echo "DENIED ($label): $sym is '${kind:-missing}', not a .text symbol"
            fail=1
        fi
    done
}

cargo build -q -p kernel --target x86_64-unknown-none
check "default build" target/x86_64-unknown-none/debug/kernel

CARGO_PROFILE_DEV_CODEGEN_UNITS=1 cargo build -q -p kernel --target x86_64-unknown-none \
    --target-dir target/asm-check
check "one codegen unit" target/asm-check/x86_64-unknown-none/debug/kernel

if [[ "$fail" -ne 0 ]]; then
    echo
    echo "a global_asm! block probably left the section switched; use .pushsection/.popsection"
    exit 1
fi
echo "asm sections OK: every global_asm! entry point is in .text"
