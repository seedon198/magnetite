# 0007: `unsafe` policy: forbid outside `hal`

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

Rust ([ADR 0001](0001-implementation-language.md)) guarantees memory safety
only for *safe* code; `unsafe` blocks opt out and are where memory-safety bugs
can still occur. A kernel unavoidably needs some `unsafe` (MMIO, MSRs, page
tables, inline asm). The security value of Rust depends on how tightly that
`unsafe` is contained and reviewed. We want the strongest discipline the
project can sustain.

## Options considered

1. **Pragmatic unsafe**: use `unsafe` wherever convenient, wrapped in safe
   APIs. Fastest, normal practice - but `unsafe` ends up scattered across many
   crates, enlarging the audit surface.
2. **`forbid(unsafe_code)` outside a single audited `hal` crate**: all raw
   hardware access is concentrated in `hal`; the kernel, servers and tools
   cannot write `unsafe` at all (the compiler rejects it). Every `unsafe` block
   in `hal` carries a `// SAFETY:` invariant and is reviewed. Strong guarantee;
   requires designing clean safe abstractions over hardware.
3. **Formal/verified unsafe** (Kani, Miri-proven, Prusti): highest assurance,
   but research-grade effort that would dominate early development.

## Decision

**Option 2.** Every crate declares `#![forbid(unsafe_code)]` except `hal` and
the kernel *binary* crate (see the exception below). `hal` is the small,
audited home of all genuine `unsafe`; each block is minimal and preceded by a
`// SAFETY:` comment justifying soundness. We additionally run **Miri** on
host-testable crates and **clippy** (`undocumented_unsafe_blocks`) in CI.
Formal verification (Option 3) remains a "beyond" goal, layered on later
without changing this policy.

### Exception: the kernel `boot` module

Two kinds of unavoidable linker glue are classified as *unsafe attributes* and
so rejected by `#![forbid(unsafe_code)]` with no possible local override:

- `#[unsafe(no_mangle)]` on the ELF entry symbol `_start`; and
- `#[unsafe(link_section = ".requests")]` on each Limine request static, which
  places them where the bootloader can find them.

Neither is memory-unsafe *code* - they are placement/linker directives - but
both need `unsafe`. They are all bootloader-handoff concerns, so they live in
one place: the kernel's **`boot` module** (`kernel/src/boot/`). That module
carries a single `#![allow(unsafe_code)]`; the kernel crate root stays
`#![deny(unsafe_code)]`, so all kernel *logic* (`kmain` and below) still cannot
use `unsafe`. Pure-logic crates (`libs/*`, and later the user-space servers and
tools) remain `#![forbid(unsafe_code)]`.

This is **CI-enforced** (`scripts/check-unsafe.sh`): outside `hal/`, an
attribute that names `unsafe_code` with any level but `forbid` or `deny` (an
`allow`, also inside a list or a `cfg_attr`, an `expect`, a `warn`) may appear
**only** within `kernel/src/boot/` and `libs/sys/src/arch.rs` (#115); every
other non-`hal` crate root must declare `forbid`/`deny(unsafe_code)`.

So the trusted, `unsafe`-bearing surface is exactly two small, reviewed places:
the `hal` crate and the kernel `boot` module.

(We chose this over a separate `boot` *crate* wrapping a `forbid` kernel
library: keeping the entry in the kernel binary is less churn, and the
guarantee - no `unsafe` in kernel logic, all glue confined and CI-checked - is
the same.)

## Consequences

- The dangerous surface is two small, reviewable places (`hal` and the kernel
  `boot` module), instead of the whole tree - a concrete, enforceable security
  property.
- Forces clean, safe abstractions over hardware, which improves design.
- Some ergonomics cost: occasionally a safe wrapper must be added to `hal`
  before higher layers can use a feature. Accepted.
- CI must enforce it (`forbid`/`deny` are per-crate; a grep guards against a
  stray `#[allow(unsafe_code)]` outside `hal/` and `kernel/src/boot/`). The grep
  matches the *attribute*, not mentions of it in comments or docs.

## Amendment (2026-11-01, #82): the user-side system-call library

User programs (ADR 0019) need a few instructions no safe Rust can express: the
`syscall` instruction itself, their `_start` entry point, and (for the
confinement test) a read through a raw address. These live in one module,
`libs/sys/src/arch.rs`, which carries the only `#![allow(unsafe_code)]` in
`libs/sys`; the crate root is `#![deny(unsafe_code)]`. User programs stay
`#![forbid(unsafe_code)]`: they name their entry through `sys::entry!`, whose
expansion the lint does not attribute to them.

`libs/sys` runs only in ring 3, so its `unsafe` cannot touch kernel memory: the
worst it can do is fault its own task. The trusted, `unsafe`-bearing surface
becomes three small places: the `hal` crate, the kernel `boot` module, and
`libs/sys`'s `arch` module. `scripts/check-unsafe.sh` allows the attribute in
`libs/sys/src/arch.rs` only, and checks the crate roots under `user/` like
every other.
