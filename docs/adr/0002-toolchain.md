# 0002: Toolchain: Rust + LLVM, QEMU

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

The primary dev machine is an Apple Silicon Mac (arm64); the target is
x86-64 bare metal. Contributors may use Linux or Windows. The toolchain must
cross-compile to x86-64 from any host, reproducibly, including in CI. Following
[ADR 0001](0001-implementation-language.md), the language is Rust.

## Options considered

- **Rust target `x86_64-unknown-none`** (stable): a Tier-2 bare-metal target
  with a precompiled `core`/`alloc`. Verified building a `no_std` kernel on
  arm64 macOS with stable `rustc` 1.94. No C cross-compiler needed.
- **Custom target JSON + `-Zbuild-std`** (nightly): needed only if we must
  change codegen (disable the red zone or SSE, set a specific relocation
  model). Requires pinning a nightly toolchain and the `rust-src` component.
- **Running:** QEMU (fast, scriptable, GDB stub, headless in CI) vs. Bochs
  (great debugger, slow) vs. VirtualBox/VMware (can't run x86 guests on Apple
  Silicon).

## Decision

- **Rust via `rustup`**, toolchain pinned in `rust-toolchain.toml` to an
  exact release (for example `1.99.1`, not the moving `stable` channel; #114),
  bumped deliberately in its own PR. Start on the stable
  `x86_64-unknown-none` target; move to a **custom target +
  `-Zbuild-std` on pinned nightly** only if codegen requirements force it, via
  a follow-up note here.
- **LLVM tools** through `rustup component add llvm-tools` for
  `objdump`/`nm`/`size`.
- **Emulator:** QEMU `qemu-system-x86_64`, machine `q35`. GDB (`x86_64-elf-gdb`)
  or LLDB via QEMU's stub.
- Bochs kept as an optional debugging aid, not a supported target.

## Consequences

- One `rustup` install cross-compiles from macOS/Linux/WSL2; no GCC cross-build.
- Pinning the toolchain keeps builds reproducible across contributors and CI.
- If/when we need the red zone disabled (for interrupt handlers) or SSE off
  before FPU state is saved, we switch to a custom target spec; this is an
  expected, documented step, not a surprise.
