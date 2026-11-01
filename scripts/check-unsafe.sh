#!/usr/bin/env bash
# Enforce the unsafe-code policy (ADR 0007): `unsafe` lives only in the `hal`
# crate, with exactly one audited exception - the `#[allow(unsafe_code)]` on the
# kernel's `_start` entry shim (the ELF entry needs `#[unsafe(no_mangle)]`,
# which `forbid` cannot permit).
#
# The compiler already enforces most of this: non-`hal` crates declare
# `#![forbid(unsafe_code)]` or `#![deny(unsafe_code)]`, so real `unsafe` code
# will not compile there. This script is defense in depth against someone
# sneaking in an extra `#[allow(unsafe_code)]` or weakening a crate's lint.
#
# Exit non-zero on any violation. Portable bash (macOS and Linux).
set -euo pipefail

cd "$(dirname "$0")/.."

fail=0

# Crates/paths permitted to contain `unsafe`:
#   - the `hal` crate (hardware access)
#   - the kernel's `boot` module (bootloader + linker glue: the entry point and
#     Limine request placement)
#   - `libs/sys` (the user side of `syscall` and the user entry point; it runs
#     only in ring 3; ADR 0007's 2026-11-01 amendment)
# See docs/adr/0007-unsafe-policy.md.
is_trusted() { [[ "$1" == hal/* || "$1" == kernel/src/boot/* || "$1" == libs/sys/* ]]; }

# Find every `allow(unsafe_code)` ATTRIBUTE (inner or outer), ignoring mentions
# inside comments/docs by requiring the line to start (after optional
# whitespace) with the attribute form `#[...]` / `#![...]`.
mapfile -t allow_hits < <(
  grep -REn --include='*.rs' '^[[:space:]]*#!?\[[[:space:]]*allow\([[:space:]]*unsafe_code[[:space:]]*\)' \
    kernel libs user xtask servers runtime tools 2>/dev/null || true
)

for hit in "${allow_hits[@]:-}"; do
  [[ -z "$hit" ]] && continue
  file="${hit%%:*}"
  if is_trusted "$file"; then continue; fi
  echo "DENIED: allow(unsafe_code) outside hal/, kernel/src/boot/ and libs/sys/:"
  echo "  $hit"
  fail=1
done

# Every non-`hal` crate root must declare forbid/deny(unsafe_code), so the
# compiler keeps enforcing the rule even in new crates. (`hal` is the one crate
# permitted unsafe at the crate level.)
while IFS= read -r root; do
  [[ "$root" == hal/* || "$root" == ./hal/* ]] && continue
  if ! grep -Eq '^[[:space:]]*#!\[[[:space:]]*(forbid|deny)\([[:space:]]*unsafe_code[[:space:]]*\)' "$root"; then
    echo "DENIED: crate root does not #![forbid]/#![deny](unsafe_code): $root"
    fail=1
  fi
done < <(find kernel libs user xtask servers runtime tools \( -name lib.rs -o -name main.rs \) 2>/dev/null)

if [[ "$fail" -ne 0 ]]; then
  echo
  echo "unsafe-code policy violation(s) found. See docs/adr/0007-unsafe-policy.md"
  exit 1
fi

echo "unsafe-code policy OK: unsafe confined to hal/, kernel/src/boot/ and libs/sys/"
