# Threat Model

> Status: **living document.** Revised as the system grows.

This OS is both a *hardened* system and a *platform for offensive security
work*. Those two roles have different threat models; this page keeps them
straight, and sets the ethical boundary.

## Intended use

This is software for **authorized** security testing, research and education:
testing systems you own or have written permission to test, CTFs, lab work,
and learning OS/security internals. It is **not** for unauthorized access to
systems or data. See [SECURITY.md](../../SECURITY.md).

## Part 1 - Threats *to* the OS (what we harden against)

The memory-level mitigations below (W^X, NX, SMEP/SMAP, KASLR, guard pages, zeroed
frames, per-address-space isolation) are **implemented as of milestone M2** - see
[memory-management.md](memory-management.md). A ✓ marks mitigations already in the
kernel; others are design commitments realized in later milestones.

| Asset | Threat | Mitigation |
|---|---|---|
| Kernel integrity (TCB) | Exploit in a driver/parser escalating to ring 0 | Microkernel: drivers/parsers are isolated user servers (ADR 0004); kernel is safe Rust (ADR 0001/0007) |
| Memory safety | UAF, buffer overflow, type confusion | Rust `forbid(unsafe)` outside `hal`; the few `unsafe` blocks audited ✓ |
| Task isolation | One task reading/writing another's memory | Per-address-space paging ✓ (own page tables, M2); capability model - no ambient authority, attenuation-only ✓ (M3); user tasks run in ring 3 in their own address spaces, and a fault there ends only that task ✓ (M4, #82) |
| CPU availability | A thread that never yields (buggy or hostile) starving every other thread | LAPIC-timer preemption, 10 ms round-robin slices ✓ for kernel threads (M4, #80) and for ring 3, on per-thread kernel stacks ✓ (#82); interrupt-safe locks keep preemption deadlock-free |
| Code injection | Overwriting code, running injected code | W^X + NX enforced and verified; HHDM non-executable; SMEP/SMAP enabled ✓ (M2). `Frame` caps separate map/exec rights (later) |
| Exploit reliability | Predictable kernel addresses | Kernel KASLR ✓; stack guard pages ✓; zeroed-on-alloc frames ✓ (M2) |
| Hostile input | Malicious packets, filesystems, images | Parsed only in sandboxed servers; fuzzed in CI |
| Supply chain | Compromised dependency | Minimal deps; `cargo deny` / `cargo audit`; pinned toolchain; vendored Limine ✓ |

### Trust boundaries

- **Fully trusted:** the microkernel and `hal`. Correctness here is required
  for all security. Kept small and reviewed.
- **Confined:** every user server, by its capability set. Assumed buggy;
  contained by design.
- **Hostile:** all external input (network, storage, USB) and anything a
  native tool interacts with on a target.

## Part 2 - The OS *as* a security tool (what it is allowed to do)

The system intentionally provides capabilities that are "offensive":

- raw packet transmit/receive and promiscuous capture,
- port/host scanning primitives,
- a fuzzing harness and scripting runtime for writing tooling.

These are gated the same way as everything else - a task must **hold the
capability** to use them (see [security model](security-model.md)). They are
powerful, not unconfined.

### Ethical and legal boundary

- Offensive capabilities are for use against systems the operator is
  **authorized** to test.
- The project will not ship features whose only purpose is to cause harm to
  non-consenting third parties (e.g. self-spreading worm logic, botnet C2,
  destructive payloads, mass-exploitation automation aimed at the public
  internet). Dual-use primitives (raw sockets, scanners, fuzzers) are in
  scope; weaponized malware is not.
- Contributions are expected to respect this boundary; see
  [CONTRIBUTING.md](../../CONTRIBUTING.md) and [SECURITY.md](../../SECURITY.md).

## Out of scope (currently)

- Physical attacks (DMA/Thunderbolt, cold boot, evil maid) beyond basic IOMMU
  use later.
- Microarchitectural side channels (Spectre/Meltdown-class).
- Defending the *targets* of the offensive tooling - that is the user's
  responsibility and authorization.
