# 0004: Kernel architecture: microkernel

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

The kernel structure determines the size of the **Trusted Computing Base** -
the code that must be correct for the system to be secure. For a
security-focused kernel this is the central architectural decision. The kernel
also hosts offensive work (drivers and a network stack handling hostile input),
which makes isolation of those components especially valuable.

## Options considered

1. **Monolithic + hardening**: everything in ring 0 with W^X, KASLR,
   SMEP/SMAP. Simplest and fastest, and Rust removes many bugs - but a single
   exploited driver or parser still means full kernel compromise. Large TCB.
2. **Microkernel**: ring 0 provides only address spaces, threads, capabilities
   and IPC; drivers, network stack and filesystems are isolated user-space
   servers. Smallest possible TCB; a compromised server is confined to its
   capabilities. More upfront design (IPC, servers) and attention to IPC
   performance.
3. **Capability microkernel, seL4-style**: as above, with a rigorously minimal,
   potentially verifiable core. Strongest assurance, highest design cost.
4. **Hybrid**: monolithic core with only the highest-risk parsers isolated.
   Pragmatic, but leaves a medium TCB and a fuzzier security boundary.

## Decision

A **microkernel**. The privileged core is tiny and written in safe Rust;
drivers, the network stack, filesystems and the security-tooling runtime run as
isolated, least-privilege user-space servers. The design draws on seL4's
capability model ([ADR 0006](0006-security-model.md)) without committing to
formal verification now (left as a "beyond" goal).

## Consequences

- Minimal TCB directly serves the security goal and the
  [threat model](../architecture/threat-model.md): a bug in a packet parser
  cannot reach ring 0.
- The offensive network/driver code - which handles the most hostile input -
  is sandboxed by construction.
- Costs: we must design and build IPC and a server model (M3-M5) before the
  system is broadly useful, and keep IPC fast. Accepted as core to the project.
- Keeps a future move toward verification of the core open.
