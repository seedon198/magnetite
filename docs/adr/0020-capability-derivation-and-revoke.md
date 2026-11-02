# 0020: Capability derivation tree and revoke

- **Status:** Proposed
- **Date:** 2026-11-02

## Context

A kernel object lives in memory retyped from an Untyped region
([ADR 0016](0016-capability-implementation.md)). Reclaiming that memory is
safe only when no capability anywhere still names an object inside it.
Otherwise the capability reaches whatever reuses the frames, which is kernel
memory corruption from ring 3 (#105).

What exists today, and limits this design:

- There is **no revoke and no derivation tracking**. A capability does not know
  where it came from, and nothing can find the copies made from it.
- **Reclaim is guarded by a scan.** `reap_root` frees a task's memory only if
  `ipc::find_reference` finds no capability in any capability space naming an
  object inside it, and otherwise keeps the memory (PR #146). This is safe, but
  it cannot *take back* a capability: a server that keeps one pins the client's
  memory for ever.
- **Today it holds only because no capability with `GRANT` leaves a task.** The
  first `GRANT` path between tasks (M5 servers) makes the scan's refusal the
  normal case, not the exception.
- **`CNodeDelete` never fails threads blocked on an endpoint.** It cannot happen
  yet: a blocked thread cannot delete its own capability, and only a task holds
  its own CNode capability. It can once CNode capabilities are shared.
- IPC transfer **copies** a capability: the sender keeps its own, and the
  receiver gets an equal or weaker one.
- Capability spaces are flat per-thread tables (`CapSpace`, ADR 0016) behind
  one lock, `CSPACES`. The lock order is IPC, CSPACES, SCHED, SPACES,
  KERNEL_SPACE, frame allocator.
- Syscalls run with interrupts off (`FMASK`), on a single core.

The maintainer chose this direction in the #105 discussion ("guard now, a
capability derivation tree later"), over per-object version numbers
(KeyKOS/EROS style), which can revoke only every capability to an object at
once, never one derived branch.

## Options considered

1. **seL4's mapping database: an ordered list with depth.** Every slot holds
   `prev` and `next` links and a `depth`. The list is a pre-order walk of the
   derivation tree, so a capability's descendants are the entries right after
   it with a greater depth. Revoke deletes that run. Deleting one entry lifts
   its descendants one level (to its parent) by decrementing their depth.
   - Pros: two links per slot. Every capability to one object sits in one
     contiguous run, so "is this the last capability to the object" is a
     check of the two neighbours. The design is proven in seL4.
   - Cons: deleting an entry costs time proportional to its subtree, for the
     depth fix-up.
2. **An explicit tree: parent, first child, next sibling.** Delete splices
   the children up to the parent in O(children).
   - Cons: three links per slot, and more link updates per operation. Finding
     the other capabilities to the same object needs a walk, not a neighbour
     check.
3. **Per-object version numbers.** Revoke is O(1), checked on each use.
   - Cons: no selective revoke. The version word must live outside reclaimable
     memory. Rejected by the maintainer.
4. **Keep the scan; add reference counts.** No revoke at all, so a holder can
   pin another task's memory for ever (denial of service by hoarding).

## Decision

### 1. The mapping database: an ordered list with depth (option 1)

Each slot grows from its capability alone to the capability plus a link
record:

```
Slot { cap: Option<Capability>, prev: SlotRef, next: SlotRef, depth: u16 }
```

A `SlotRef` names a slot anywhere in the system as `(cnode, index)`:
- `cnode` is an index into a kernel **CNode registry**: a bounded table of
  live CNodes, filled at retype and cleared at destroy.
- `index` is the slot's index in that CNode.

Links are never raw pointers, so the database stays in safe Rust: the pure
crate resolves a `SlotRef` through the registry, and nothing aliases a
`CapSpace`'s slots.

The registry also issues **typed object references** (`EndpointRef`,
`CNodeRef`, and so on), and `hal::object` takes those instead of bare
physical addresses. A physical address of one kind can then no longer be
used as another, and `cnode_cap_space` consumes its `CNodeRef` instead of
handing out an aliasable `CapSpace<'static>` (the rest of #106; until then,
every `hal::object` access checks that it lies in allocated pool frames).

Invariant: the list is a pre-order walk of the derivation tree. The children
of an entry at depth `d` are the following entries at depth `d + 1`, up to
the first entry at depth `<= d`.

### 2. What derives from what

| Operation | New capability's place in the database |
|---|---|
| `UntypedRetype` | a child of the Untyped capability, one per object created |
| `CNodeCopy`, `CNodeMint` | a child of the source capability |
| IPC transfer (`GRANT`) | a child of the sender's capability |
| `CNodeMove` | moves the entry itself; its place in the tree is unchanged |
| Bootstrap (kernel-made capabilities) | roots at depth 0, or children of a kernel-held root Untyped |

A derived capability is never stronger than its parent (ADR 0016), so every
capability's authority traces back through its ancestors to an original.

### 3. Delete, revoke, and the last capability

- **`CNodeDelete(slot)`** removes one entry. Its descendants move up to its
  parent: their depth drops by one. They stay valid; deleting a capability
  does not take back what was derived from it. Only revoke does that.
- **`CNodeRevoke(slot)`** (new method, needs `WRITE` on the CNode) deletes
  every descendant of the entry, but not the entry itself. Revoking a minted,
  badged endpoint capability takes back exactly the copies made from it. That
  is seL4's selective revoke, which version numbers cannot express.
- **The last capability to an object.** When an entry is deleted and neither
  list neighbour names the same object, the object is **destroyed**:
  - Endpoint: every thread queued on it is woken with a new error,
    `IpcError::Revoked` (`SysError::Revoked`).
  - Notification: every waiter is woken the same way, once waits exist.
  - CNode: each of its slots is deleted, recursively, and it leaves the
    registry.
  - Frame and AddressSpace: unmapped wherever they are mapped. That needs
    mapping back-pointers, so it lands with those objects' user-mappable
    forms; until then, their capabilities cannot leave the task.

  This replaces "fail waiters when the last endpoint capability is deleted",
  which PR #146 deferred to here.
- **Revoking an Untyped** destroys every object retyped from it, then resets
  its watermark to the region's base, so the memory is reusable. Reaping a
  task becomes: revoke its root Untyped, delete its remaining capabilities,
  then free the region. `ipc::find_reference` stays as a debug assertion that
  revoke left nothing behind.

### 4. Bounded work: revoke is restartable

One revoke can delete an unbounded subtree with interrupts off. So each
`CNodeRevoke` call does at most **64 deletions** and then returns
`SysError::Again`, and the caller invokes it again until it returns `Ok`.
Revoke always deletes the deepest, last descendant first, so a partial revoke
leaves a valid tree, and the next call continues where the last one stopped.
The kernel's own reap loops the same way, and yields between rounds. A
recursive CNode destroy is broken into the same steps.

### 5. Locking

Revoke and the final delete can wake threads blocked in IPC. So every
capability operation that may destroy an object takes **IPC, then CSPACES**,
following the existing order:
- `CNodeDelete` and `CNodeRevoke` in `invoke`.
- IPC transfer already runs inside the IPC lock.

`with_cspace` callers that only read keep taking CSPACES alone.

### 6. Where the code lives

- **The database is pure logic** in the `capability` crate,
  `#![forbid(unsafe_code)]`:
  - insert-as-child, delete-with-lift, revoke-step, and the last-capability
    check;
  - written over a small `SlotStore` trait (get and set a slot by
    `SlotRef`), so host tests run it on plain arrays and the kernel binds
    it to the CNode registry.
- **Host tests:**
  - the pre-order and depth invariant after every operation, over
    randomized operation sequences;
  - selective revoke of a badged branch;
  - a restartable revoke cut off after every possible step;
  - the last-capability detection.
- **Boot checks reproduce the #105 failure scenario** end to end:
  - A task retypes an Endpoint and grants it to a server. The task is reaped,
    and its memory is freed only after revoke. The server's next send fails
    with `InvalidCap`.
  - A server blocked on that endpoint wakes with `Revoked`.

### 7. Sizes

A slot grows by two `SlotRef`s and a `depth`, about 18 bytes, padded to a
multiple of 8. The root task's 64-slot CNode still fits in one page.
`cnode_bytes` and `slot_align` follow the new type. The syscall ABI names
slots by index, so it does not change, apart from the new `CNodeRevoke`
method and the `Revoked` and `Again` error codes.

## Consequences

- A capability can be taken back: selectively (one badged branch) or wholesale
  (an Untyped and everything made from it). Reaping a task no longer depends
  on its peers giving its capabilities back, so the scan's refusal path
  disappears outside debug checks.
- The first `GRANT` path between tasks is safe to build. **This ADR must be
  implemented before any `GRANT` path between tasks exists.**
- Every capability operation updates links in up to three slots, often in
  different CNodes. Capability operations get slower and more intricate. The
  pure, invariant-checked core is the mitigation.
- Revoke is restartable, so user code must loop on `Again`. `libs/sys` will
  offer a `revoke` helper that does.
- **Revisit with SMP:** one CSPACES lock serializes all capability operations.
  That is fine on one core, but per-CNode locking needs its own design.
- **Revisit with Frame mapping:** destroying a mapped Frame needs reverse
  mappings. Until then, Frame and AddressSpace capabilities stay inside their
  task.
