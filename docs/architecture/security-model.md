# Security Model

> Status: the **model is implemented in milestone M3**
> ([ADR 0006](../adr/0006-security-model.md), [ADR 0016](../adr/0016-capability-implementation.md));
> see [capabilities.md](capabilities.md) for how. M4 added blocking IPC
> ([ipc.md](ipc.md)) and ring-3 tasks behind the `syscall` boundary
> ([syscalls.md](syscalls.md), [scheduling.md](scheduling.md)): a task reaches
> the kernel only by invoking a capability it holds.

The system is built around **capabilities** and **least privilege**. This page
describes the model; [capabilities.md](capabilities.md) describes the
implementation (the cap algebra, cap space, Untyped/retype, object kinds, and
the invocation API).

## Principles

1. **No ambient authority.** A task's power is exactly the set of capabilities
   in its capability space - nothing more. There is no global namespace, no
   "root", no implicit access to devices, memory or other tasks.
2. **Unforgeable handles.** A capability cannot be guessed or fabricated. It is
   a kernel-protected reference, named by an index into a per-task table; the
   object it points to and the rights it carries live in the kernel.
3. **Explicit delegation.** Authority spreads only by a holder explicitly
   sending a capability over IPC. Privilege flows are therefore visible and
   can be audited.
4. **Attenuation.** A capability can be *derived* into a weaker one (fewer
   rights, a sub-range), never a stronger one.
5. **Small TCB.** Only the microkernel and `hal` are fully trusted. Everything
   else is confined by the capabilities it holds.

## Objects (initial set)

| Object | Represents | Example rights |
|---|---|---|
| `Untyped` | unused physical memory | retype into other objects |
| `Frame` | a physical page | map, read, write |
| `AddressSpace` | a page table root | map/unmap frames |
| `Thread` | a schedulable context | start, stop, set registers |
| `Endpoint` | an IPC rendezvous | send, receive |
| `Notification` | async signal | signal, wait |
| `IrqHandler` | a hardware IRQ line | acknowledge, bind |
| `IoPort` / `Mmio` | device I/O region | read, write (granted to driver servers) |

Rights are bits on the capability; a driver gets an `Mmio` cap for *its*
device's registers and nothing else.

## How a driver works (illustrative)

1. The initial task (`root server`) is handed a small set of capabilities by
   the kernel at boot (all `Untyped` memory, the IRQ control cap, etc.).
2. To start the network driver, `root` carves out memory, creates an address
   space and thread, and grants the driver **only**: its NIC's `Mmio` region,
   the NIC's `IrqHandler`, and an `Endpoint` to talk to the net server.
3. The driver cannot touch any other device, any other task's memory, or the
   kernel. If it is exploited, the blast radius is that one `Mmio` region.

## Mapping to offensive tooling

Powerful primitives are still just capabilities:

- **Raw packet access / promiscuous mode** is a right on the NIC driver's
  service endpoint, granted deliberately to the security runtime.
- The **scripting runtime** ([ADR 0008](../adr/0008-native-tooling.md)) holds
  only the capabilities it is given, so a tool that scans or sniffs is
  contained exactly like any other server. "Powerful" never means "unconfined".

## Hardware-enforced hardening (defense in depth)

Capabilities are the software model; the CPU features back them up:

- **W^X / NX** - no page is both writable and executable.
- **SMEP / SMAP** - the kernel cannot execute or (without an explicit window)
  read user memory.
- **KASLR** - the kernel image is loaded at a randomized base.
- **Guard pages** around kernel stacks; stack probes.
- **Memory safety in Rust** removes the bug classes (UAF, overflow, type
  confusion) that most exploits depend on in the first place.

## Explicit non-goals (for now)

- Multi-user accounts / Unix permissions - the capability model replaces them.
- Mandatory access control labels (SELinux-style) - capabilities subsume them.
- Side-channel resistance (Spectre-class) - tracked separately, not M-series.
