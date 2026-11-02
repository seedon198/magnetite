# 0001: Implementation language: Rust

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

Magnetite is a **security-focused** kernel, and being hardened is its main
selling point. The single largest source of kernel vulnerabilities across the
industry is memory-unsafety: use-after-free, buffer overflows, type confusion.
The language choice is therefore a security decision, not just an ergonomics
one. The target is bare-metal x86-64; development is on Apple Silicon macOS.

## Options considered

1. **C17**: universal in OS dev, best-documented path, but provides no memory
   safety. Every exploit class we most want to eliminate is wide open, and we
   would spend ongoing effort mitigating bugs the language permits.
2. **Rust (`no_std`)**: memory and thread safety enforced by the compiler,
   with no garbage collector and no mandatory runtime. Cross-compiles to
   `x86_64-unknown-none` out of the box (verified: `rustc` 1.94 builds and
   links a `no_std` x86-64 kernel on arm64 macOS). Mature OS-dev ecosystem
   (`x86_64`, `bootloader`, Philipp Oppermann's guide, Redox, seL4 bindings).
   Cost: steeper learning curve, and some bare-metal work needs `unsafe`.
3. **C core + Rust drivers**: hedges, but means two toolchains and two sets of
   idioms, and leaves the core (the most security-critical part) unsafe.
4. **Zig**: attractive and simple, but pre-1.0 and lacks Rust's safety
   guarantees and ecosystem.

## Decision

**Rust**, `no_std`, for the kernel, the HAL, all servers and the tooling. The
danger of `unsafe` is managed by [ADR 0007](0007-unsafe-policy.md), which
confines it to the `hal` crate.

## Consequences

- Whole vulnerability classes are removed from safe code by construction,
  directly serving the security goal and the [threat model](../architecture/threat-model.md).
- We trade some documentation breadth (most OSDev examples are C) for safety;
  the Rust OS-dev ecosystem and `hal` isolation offset this.
- The learning curve is real but aligned with the project's purpose.
- Nightly Rust may be required for `-Zbuild-std` if a custom target spec is
  needed; pinned in `rust-toolchain.toml` (see [ADR 0002](0002-toolchain.md)).
