# 0005: Build system: Cargo + xtask

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

The project is a Rust workspace (kernel, HAL, servers, tools). Beyond compiling
Rust, the build must fetch and run Limine, assemble a bootable ISO, launch
QEMU, and attach a debugger - on macOS, Linux and CI.

## Options considered

1. **Cargo alone**: compiles the crates, but can't express "build ISO, run
   QEMU, fetch Limine". We'd need shell scripts around it (non-portable,
   especially macOS vs. Linux `sed`/`dd`).
2. **GNU Make wrapping Cargo**: familiar, but macOS ships Make 3.81, adds a
   second language, and reimplements logic Cargo already knows.
3. **`cargo xtask`**: the community-standard pattern - a plain Rust binary in
   the workspace (`cargo xtask <cmd>`) that orchestrates everything. The build
   automation is itself type-checked Rust, cross-platform, and needs no tool
   beyond the Rust toolchain already required.

## Decision

**Cargo + an `xtask` crate.** `cargo xtask build | run | debug | iso | test`
drives compilation, Limine fetch/pin, ISO assembly (via `xorriso`), and QEMU.
The kernel target and flags live in Cargo config / a target spec; `xtask` holds
the orchestration.

## Consequences

- One toolchain (Rust) for both the kernel and its build logic; nothing extra to
  install beyond `qemu` and `xorriso`.
- Cross-platform without shell-script portability traps.
- Slightly more verbose than a `Makefile` for trivial tasks, and `xtask` is a
  little boilerplate up front. If the build outgrows it, revisit with a new ADR.
