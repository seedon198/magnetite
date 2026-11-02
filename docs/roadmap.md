# Roadmap

Development is organized into **milestones**. Each ends in a state that boots
in QEMU (and, where relevant, on real hardware) and passes CI. Tracked on
GitHub: <https://github.com/seedon198/magnetite/milestones>.

**Keeping this current:** a PR that completes a roadmap item ticks it here in
the same PR, and a release that closes a milestone marks its status line.

| Milestone | Status |
|---|---|
| M0 Foundation | done, [v0.0.1](https://github.com/seedon198/magnetite/releases/tag/v0.0.1) |
| M1 Kernel core | done, [v0.1.0](https://github.com/seedon198/magnetite/releases/tag/v0.1.0) |
| M2 Memory & isolation | done, [v0.2.0](https://github.com/seedon198/magnetite/releases/tag/v0.2.0) |
| M3 Capabilities | done, [v0.3.0](https://github.com/seedon198/magnetite/releases/tag/v0.3.0) |
| M4 Scheduling & userspace | in progress |
| M5 to M7 | planned |

Two themes run through every milestone:

- **Hardening** - the privileged core stays tiny and memory-safe; every
  addition is justified against the [threat model](architecture/threat-model.md).
- **Offense enablement** - the system is steadily shaped toward running native
  security tooling.

---

## M0: Foundation

**Status: done** ([v0.0.1](https://github.com/seedon198/magnetite/releases/tag/v0.0.1)).

*Goal: a Rust `no_std` kernel boots via Limine and prints "hello" over serial;
`cargo xtask run` does it in one command; CI proves it.*

- [x] Workspace skeleton (`kernel`, `hal`, `xtask`, `libs`) and `rust-toolchain.toml`
- [x] `x86_64-unknown-none` build with `panic=abort` (no custom target or `build-std` needed)
- [x] Higher-half linker script; Limine protocol requests
- [x] `_start` → `kmain`, base-revision check, halt loop
- [x] 16550 UART serial driver (in `hal`) for early output
- [x] `cargo xtask {build,run,debug,iso}` wrapping Limine + xorriso + QEMU
- [x] CI: build, `cargo clippy -D warnings`, `rustfmt --check`, QEMU boot smoke test (UEFI and BIOS)
- [x] Unsafe policy enforced from commit one: `kernel` is `deny(unsafe_code)`, with linker glue only in its `boot` module ([ADR 0007](adr/0007-unsafe-policy.md))

## M1: Kernel core

**Status: done** ([v0.1.0](https://github.com/seedon198/magnetite/releases/tag/v0.1.0)).

- [x] `hal`: GDT/TSS, IDT, all CPU exception handlers (the audited unsafe core)
- [x] `panic` handler with a backtrace; exception handlers dump registers
- [x] Structured logging (levels; serial and framebuffer sinks)
- [x] Parse the Limine memory map and CPU features (NX, SMEP, SMAP, x2APIC)
- [x] Host-testable core logic in pure `libs/` crates

## M2: Memory & isolation

**Status: done** ([v0.2.0](https://github.com/seedon198/magnetite/releases/tag/v0.2.0)).
See [architecture/memory-management.md](architecture/memory-management.md).

- [x] Physical frame allocator (bitmap)
- [x] Virtual memory: own page tables, per-address-space isolation
- [x] **W^X** enforced, **NX** everywhere (incl. HHDM), **KASLR** for the kernel image
- [x] SMEP/SMAP enabled; guard pages; kernel heap
- [x] Page-fault diagnostics
- [x] `architecture/memory-management.md`; threat model updated

## M3: Capabilities & IPC

**Status: done** ([v0.3.0](https://github.com/seedon198/magnetite/releases/tag/v0.3.0)).
*The microkernel's heart. See [security model](architecture/security-model.md)
and [capabilities.md](architecture/capabilities.md).*

- [x] Capability object model (unforgeable handles, rights, derivation)
- [x] Per-task capability space; no ambient authority (flat table / CNode)
- [x] Untyped memory & retype; core object kinds
- [x] Capability invocation API (rights-checked `invoke`)
- [x] ADR finalizing the capability design ([ADR 0016](adr/0016-capability-implementation.md))

Synchronous IPC (blocking `Endpoint` send/receive/call) and the ring-3
`syscall` entry need threads/userspace and are part of **M4**.

## M4: Scheduling & userspace

**Status: in progress** (epic #19).

- [x] Threads, context switch, round-robin scheduler (#78)
- [x] Preemptive scheduling via the LAPIC timer (#80)
- [x] Ring 3 entry/return (`syscall`/`sysret`) with SMEP/SMAP (#79)
- [x] Blocking synchronous IPC: `Endpoint` send/receive/call, capability transfer (#81)
- [x] First user task running under a restricted capability set (#82)
- [ ] FPU/SSE state save and restore (#83)
- [ ] Docs: threads, scheduling, IPC and the syscall boundary (#84)

## M5: User servers

**Status: planned.**

- [ ] Capability revoke: the derivation tree of [ADR 0020](adr/0020-capability-derivation-and-revoke.md) (#156), before any `GRANT` path between tasks
- [ ] Notification wait, Untyped splitting, user-created `Thread` and `AddressSpace` objects (#162)
- [ ] Server runtime library (`libs/`): receive-loop, capability plumbing
- [ ] Device manager server; bus enumeration (PCI/PCIe via ECAM)
- [ ] Driver-as-server model proven end to end (e.g. serial or a timer server)

## M6: Devices & network

**Status: planned.**

- [ ] virtio drivers (net, block) as servers - QEMU bring-up
- [ ] Real NIC drivers (Intel e1000e / Realtek) for generic laptops
- [ ] NVMe and xHCI (USB) drivers as servers
- [ ] **Sandboxed** network stack (Ethernet/ARP/IPv4/IPv6/TCP/UDP) in a server
- [ ] **Raw packet / promiscuous-mode** capability for offensive tooling

## M7: Security runtime & native tools

**Status: planned.**

- [ ] Embedded scripting runtime in a user-space service
- [ ] Capability-mediated API surface exposed to scripts (raw sockets, etc.)
- [ ] First native tools: port scanner, packet sniffer, a basic fuzzer harness
- [ ] Tooling that is auditable and runs without recompiling the OS

## Beyond

Wireless (802.11 + firmware - hard), storage filesystems, a shell, a package
format for native tools, reproducible builds, optional formal verification of
the capability core, self-hosting.

## Hardening checklist (continuous)

- [x] No `unsafe` outside `hal` and the kernel's `boot` glue (CI: `scripts/check-unsafe.sh`); every `unsafe` block carries a `// SAFETY:` invariant (clippy `undocumented_unsafe_blocks`)
- [x] `cargo clippy -D warnings`, `cargo deny`, `cargo audit` in CI
- [ ] Miri on host-testable crates; fuzzing on all parsers
- [x] W^X, NX, KASLR, SMEP/SMAP, guard pages
- [ ] Stack probes; a guard page on the boot stack (#66)
- [ ] Documented, minimal TCB; each privileged addition reviewed against the threat model (ongoing)
