# Capabilities

> Status: **the model is implemented in milestone M3.** The capability data
> model, Untyped/retype, the object types, and the invocation API are in the
> kernel; **blocking IPC and the ring-3 `syscall` entry are M4** (they need
> threads). See the [security model](security-model.md) for the *why* and
> [ADR 0006](../adr/0006-security-model.md) / [ADR 0016](../adr/0016-capability-implementation.md)
> for the decisions.

Authority in this kernel is exactly the set of capabilities a task holds - no
ambient authority, no "root". A capability is an unforgeable reference to a
kernel object plus the rights over it; it can only be **attenuated** when
derived, never widened. This page describes how that model is built.

As throughout the kernel, the pure algebra lives in a `#![forbid(unsafe_code)]`,
host-tested crate (`capability`), and the parts that touch physical memory live
in `hal`.

<p align="center">
  <img src="../assets/capabilities.svg" alt="Capability model: a task's capability space points at kernel objects retyped from Untyped memory; derived capabilities have fewer rights; every action is a rights-checked invoke" width="820">
</p>

## Rights and capabilities

`Rights` is a set of bit flags (`READ`, `WRITE`, `EXECUTE`, `MAP`, `SEND`,
`RECV`, `GRANT`, `RETYPE`). A `Capability<Obj>` is an object handle plus its
rights and an optional **badge** (an identifier an endpoint's receiver sees).

- `derive(want)` yields a weaker capability holding exactly `want` - only if
  `want` is a subset of the current rights (attenuation; never widening).
- `mint(want, badge)` derives and stamps a badge once.

The kernel instantiates `Capability<ObjectRef>`, where `ObjectRef { kind, phys }`
is a **tagged physical reference**: the object's kind plus the physical address
of its storage. Mutable object state lives in that storage (reached via the
HHDM), so copies of a capability share one object.

## Capability space

Each task's capabilities live in a **flat table** of slots; a `CapId` is an
index (O(1) lookup). Operations - `insert`/`get`/`delete`/`move`/`copy`/`mint` -
enforce the rules: a slot fills only when empty, and `copy`/`mint` may only
attenuate. A **CNode** is the object form of such a table: retyped storage
holding a `Slot` array, accessed via the HHDM (`hal::object::cnode_cap_space`).
A growable, two-level space is composed from CNode capabilities.

## Untyped memory and retype

Objects are created by **retyping Untyped memory** - the kernel never
heap-allocates objects on a task's behalf, so a task spends its own memory
budget and cannot exhaust kernel memory.

- `Untyped { base, size, watermark }` is a bump allocator over a physical
  region; `allocate(size, align, count)` carves naturally-aligned storage
  (checked for alignment, overflow, and exhaustion). Reclamation is
  whole-region only. There is **no revoke yet** (#105): when a task is
  reaped, the kernel scans every capability space and frees the task's
  CNode, Untyped control block and Untyped region only if no capability
  anywhere still names an object inside them (`ipc::find_reference`);
  otherwise the memory stays allocated and the holder is reported. A
  seL4-style capability derivation tree, with revoke that also fails
  threads blocked on a revoked endpoint, is planned before the first `GRANT`
  path between tasks.
- `hal::object::retype` carves from an Untyped and **zeroes** the storage
  through the HHDM, so a retyped object never exposes prior contents.
- An Untyped capability points at a small `repr(C)` control block holding its
  `Untyped` state, so the watermark persists across invocations.

## Object kinds

| Kind | Represents | Storage | Status |
|---|---|---|---|
| `Untyped` | unused physical memory | control block + region | retype works |
| `CNode` | a capability-space node | `Slot` array | backs the cap space |
| `Frame` | a physical page | one page | create + identify |
| `Endpoint` | a synchronous IPC rendezvous | its blocked-thread queue | IPC works between kernel threads (#81); user mode in #82 |
| `Notification` | an async signal | signal word + waiters | signal works; wait is M4 |
| `AddressSpace` | a page-table root | a PML4 | object only; user mapping is M4 |
| `Thread` | a schedulable context | - | M4 |

Each kind's storage `(size, align)` comes from `ObjectKind::fixed_layout` (or
`cnode_bytes` for a CNode).

## Invocation

Everything a task does is **invoke a capability** with a typed `Method`:

```
invoke(cspace, invoked, method) -> Result<InvokeOk, InvokeError>
```

The kernel resolves the invoked capability in the cap space, **authorizes** it
(`Method::authorize`: the capability must be the method's target kind and hold
its required rights), then performs the effect. Methods implemented in M3:

- `CNodeCopy` / `CNodeMint` / `CNodeMove` / `CNodeDelete` - manage capabilities
  (attenuation enforced).
- `FrameIdentify` - return a frame's physical address.
- `UntypedRetype` - carve a fixed-size object and install its capability.
- `NotificationSignal` - OR bits into a notification's signal word.

`Method::authorize` and the `Method`/error types are pure and host-tested; the
kernel dispatcher adds the memory effects. M4's ring-3 `syscall` entry decodes
raw register arguments into a `Method` and calls this same path.

## What's deferred to M4

These need threads / userspace:

- **Blocking synchronous IPC** - done for kernel threads (#81,
  [ADR 0018](../adr/0018-synchronous-ipc.md)): Send, Recv, Call, Reply and
  ReplyRecv, badges, and one-capability transfer under `GRANT`. The engine is
  the pure, host-tested `ipc` crate; user threads reach it through the
  `syscall` boundary in #82.
- **The `syscall` boundary** - the ring-3 entry that invokes capabilities.
- **`Thread`** objects, and user `AddressSpace` mapping.
- **Untyped splitting** (retyping Untyped → child Untyped) and whole-region
  **revoke**.

## Layering & testing

| Concern | Pure crate (`capability`, host-tested) | `hal` binding |
|---|---|---|
| Rights & attenuation | `Capability`, `Rights` | - |
| Capability space | `CapSpace`, `Slot`, `CapId` | `cnode_cap_space` (HHDM) |
| Untyped accounting | `Untyped`, `ObjectKind`, `ObjectRef` | `retype`, `load`/`store_untyped` |
| Invocation | `Method`, `authorize`, errors | kernel `invoke` + effects |
