# Architecture Decision Records

An ADR captures one significant decision: its context, the options considered,
and the consequences. ADRs are immutable once accepted; to change a decision,
write a new ADR that *supersedes* the old one.

Significant decisions are **discussed with the maintainer before being
accepted** - ADRs record the outcome of that discussion, they don't replace it.

| # | Title | Status |
|---|---|---|
| [0001](0001-implementation-language.md) | Implementation language: Rust | Accepted |
| [0002](0002-toolchain.md) | Toolchain: Rust + LLVM, QEMU | Accepted |
| [0003](0003-bootloader.md) | Bootloader: Limine | Accepted |
| [0004](0004-kernel-architecture.md) | Kernel architecture: microkernel | Accepted |
| [0005](0005-build-system.md) | Build system: Cargo + xtask | Accepted |
| [0006](0006-security-model.md) | Security model: capabilities | Accepted |
| [0007](0007-unsafe-policy.md) | `unsafe` policy: forbid outside `hal` | Accepted |
| [0008](0008-native-tooling.md) | Native tooling via an embedded scripting runtime | Accepted |
| [0009](0009-hand-rolled-cpu-tables.md) | Hand-roll the x86 CPU tables (no `x86_64` crate) | Accepted |
| [0010](0010-bitmap-frame-allocator.md) | Bitmap physical frame allocator | Accepted |
| [0011](0011-page-tables.md) | Own the page tables: inherit the higher half, walk via the HHDM | Accepted |
| [0012](0012-wx-nx-enforcement.md) | Enforce W^X and NX ourselves (kernel image + HHDM) | Accepted |
| [0013](0013-kernel-heap.md) | Hand-rolled growable free-list kernel heap | Accepted |
| [0014](0014-page-fault-diagnostics.md) | Page-fault diagnostics and recoverable probes | Accepted |
| [0015](0015-kaslr.md) | Kernel KASLR via a position-independent image | Accepted |
| [0016](0016-capability-implementation.md) | Capability system implementation (flat cap space, Untyped retype) | Accepted |
| [0017](0017-threads-and-scheduling.md) | Threads, context switch, and scheduling | Accepted |
| [0018](0018-synchronous-ipc.md) | Synchronous IPC over Endpoints | Accepted |
| [0019](0019-first-user-task.md) | The first user task: loading, confinement and the syscall ABI | Accepted |
| [0020](0020-capability-derivation-and-revoke.md) | Capability derivation tree and revoke | Proposed |

New ADR: copy [0000-template.md](0000-template.md), take the next number, and
open a PR labelled `adr`.
