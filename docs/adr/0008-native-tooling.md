# 0008: Native tooling via an embedded scripting runtime

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

The kernel must host a usable **offensive-security platform**, but a deliberate
constraint is **native tools only** - we are not porting Linux binaries or
providing a Linux ABI. We still need a way to write and run security tooling
(scanners, sniffers, fuzzers, custom protocol pokes) that is fast to iterate on
and does not require rebuilding and re-flashing the whole image for every change.

## Options considered

1. **Compile every tool into the image**: each tool is a native Rust server
   baked into the build. Maximum performance and type safety, but every new
   tool or tweak means a full rebuild + redeploy - painful for the
   experiment-driven nature of security work.
2. **A native shell + many small native binaries (Unix style)**: flexible, but
   requires a filesystem, a loader and a userland toolchain-on-device before
   any tooling is possible, and still recompiles per change.
3. **An embedded scripting runtime (user-space server)**: a small interpreter
   (e.g. a Lua-like or a WASM runtime) runs as a confined server; tools are
   scripts that call a capability-mediated API (raw sockets, timers, memory).
   Write/iterate tooling live, no kernel rebuild. The heavy lifting (packet I/O,
   crypto, fuzzing cores) stays in reviewed native Rust exposed to scripts.
4. **Full language runtime (Python-class)**: too large for a `no_std` from-scratch
   system in the near term.

## Decision

Build an **embedded scripting runtime** as a user-space service (Option 3),
with performance-critical primitives implemented in native Rust and surfaced to
scripts through a **capability-gated API**. The engine choice (a small
bytecode VM we write, an embeddable Lua, or a WASM runtime) is deferred to a
follow-up ADR at milestone M7, when requirements are concrete.

## Consequences

- Fast iteration on tooling without rebuilding the kernel - the right ergonomics
  for security experimentation.
- "Offense" is confined: the runtime holds only the capabilities granted to it
  ([ADR 0006](0006-security-model.md)), so a script can reach raw sockets only
  if given that capability. This keeps the [threat model](../architecture/threat-model.md)
  boundary intact.
- We must build and secure an interpreter and its API surface (a non-trivial
  attack surface itself - it will be sandboxed and fuzzed).
- Native-only means no existing-tool compatibility; we build a tool library
  over time. Accepted per the project's direction.
