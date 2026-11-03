# 0023: User-created threads and address spaces

- **Status:** Accepted
- **Date:** 2026-11-04

## Context

Issue #162 (M5): a task can create Frames, Endpoints and Notifications from
its Untyped memory, and split that memory into child Untyped regions. It
cannot create the things a new task is made of: a capability space, an
address space and a thread. Today only the kernel builds those, from boot
modules (`task::spawn`). M5's root task must start servers itself, from its
own memory, so that what it starts is bounded by what it was given and is
taken back by revoke ([ADR 0020](0020-capability-derivation-and-revoke.md)).

What exists, and limits this design:

- **Address spaces.** `hal::paging::AddressSpace` builds the user half of a
  space and takes every page-table page it needs, and every mapped frame,
  from the global frame allocator. `destroy_user` frees all of them, since
  everything in a user half has been the task's own.
- **Threads** are a fixed kernel table of 21 slots
  ([ADR 0019](0019-first-user-task.md) decision 3). Each slot has a context,
  FPU state, and optionally a kernel stack (16 KiB of frames, mapped into a
  window slot behind a guard page) and an address space.
- **CNodes** are retyped only by the kernel, which registers them in the CNode
  registry (ADR 0020). `UntypedRetype` refuses variable-size kinds.
- **Revoke and destroy** work for every object kind, but only Endpoints and
  Notifications hold kernel state to tear down.
- A capability is an object, rights, and a badge word that only Endpoint
  capabilities use.
- Single core, and syscalls run with interrupts off.

Whatever a task creates must be paid for from its own Untyped memory, or one
task could exhaust the kernel's. And everything created must be safe to
destroy at any moment, in any order, because revoke deletes a subtree
deepest-first, with no regard for what depends on what.

## Options considered

1. **seL4's model.** Page tables are objects the task retypes and installs
   level by level. A Frame or page-table capability records where it is
   mapped, so deleting it unmaps exactly that mapping. Address spaces are
   named through ASIDs, so a stale record can never touch a reused space. A
   thread's kernel memory is an object retyped from Untyped.
   - Pros: every byte is paid for by the task, and every teardown is local to
     one capability. The design is proven.
   - Cons: the most new kernel surface: four object kinds' methods, and ASIDs.
2. **The kernel allocates page tables and thread stacks** from the global pool
   on the task's behalf, with a per-task quota.
   - Pros: a much simpler user interface.
   - Cons: a second memory-accounting system beside Untyped. Quotas leak when
     tasks share frames, and revoke cannot reclaim what the kernel owns.
3. **No user-created threads in M5.** The kernel keeps loading every server
   from a boot module, and the root task only hands out capabilities.
   - Pros: no new mechanism now.
   - Cons: what a server can use is then fixed at build time. #162 and the
     roadmap call for the root task to start servers.

## Decision

Option 1, in five steps (one PR each).

### 1. CNodes from Untyped

`UntypedRetype` accepts `CNode` with a slot count (a power of two, up to 256)
in a fourth argument. The kernel retypes `cnode_bytes(slots)`, registers the
CNode in the registry, and installs its capability as a child of the
Untyped's, as for any object.

Destroying a CNode (its last capability deleted) deletes every capability in
it, destroying what each was the last capability to, then leaves the
registry. A CNode holds at most 256 slots, so this stays bounded. Destroys
that cascade into further CNodes run as a loop over a list, not as
recursion. Threads using the CNode as their capability space are stopped
first (step 4).

**CNode methods address the invoked CNode.** A task must fill a child's CNode
before the child runs, so the destination of a copy, mint or move, and the
slot a delete or revoke names, are in the CNode the invoked capability names,
while a source slot is in the caller's own space. With a capability to its
own CNode a task works on its own slots, as before.

### 2. Page tables and address spaces

- **New object kind `PageTable`** (one page, any paging level). It is retyped
  like a Frame.
- **`AddressSpace`** (one page, the PML4) is retyped with the kernel half
  copied in. It gets an **ASID**: an index into a kernel table of 64
  entries, each holding the space's root and a generation number.
- **Methods:**

  | Method | Invoked on | Arguments | Effect |
  |---|---|---|---|
  | `AddressSpaceMapTable` | AddressSpace (`MAP`) | table slot, `vaddr` | install the table at the first missing level above `vaddr` |
  | `AddressSpaceMapFrame` | AddressSpace (`MAP`) | frame slot, `vaddr`, rights | map the 4 KiB frame at `vaddr` |
  | `FrameUnmap` | Frame (`MAP`) | none | undo the frame capability's mapping |

  - **Frame mappings** need every table level above `vaddr` already
    installed, and fail with a new error, `MissingTable`, otherwise.
  - **Mapping rights** are a subset of the Frame capability's: `READ`
    always, `WRITE` for a writable mapping, `EXECUTE` for an executable
    one. `WRITE` and `EXECUTE` together are refused (W^X,
    [ADR 0012](0012-wx-nx-enforcement.md)).
  - **Addresses** must be page-aligned and in the user half.
- **Where a mapping is recorded.** A Frame or PageTable capability records
  its one mapping in its badge word, which those kinds do not otherwise use:
  ASID, generation and virtual page number, packed. A capability maps at most
  once; to map a frame twice, copy the capability.
- **PageTable capabilities cannot be copied, minted or transferred**, only
  moved, deleted and revoked. So each table has exactly one capability and is
  installed in at most one place.
- **Destroy is per capability for mappings:**
  - **Deleting a mapped Frame capability** unmaps its mapping. The entry is
    cleared only if it still maps that frame, since a table may since have
    been replaced. The TLB entry is flushed.
  - **Deleting a PageTable capability** (always the last one) clears the
    entry that installs it.
  - **Destroying an AddressSpace** stops the threads running in it, frees
    its ASID, and bumps the generation, so every mapping record that names it
    is stale and treated as unmapped.
  - **A space's memory** (root, tables and frames) belongs to the Untyped it
    came from, and comes back only by revoking that Untyped.
  - **`destroy_user`** is kept for the kernel-built spaces of boot tasks.

### 3. Kernel memory for threads

- The thread table grows to **64 slots**.
- **A `Thread` object** is 16 KiB of Untyped memory (fixed layout: four
  pages, page-aligned). Retyping one takes a free table slot and maps its
  memory as that slot's kernel stack, in the window, behind its guard page.
  Retype fails with `OutOfMemory` when no slot is free.
- **Destroying a Thread** stops it (step 4), unmaps its stack and frees its
  slot. The stack's frames go back to the Untyped, not to the frame pool.

### 4. Thread methods, and stopping a thread

| Method | Arguments | Effect |
|---|---|---|
| `ThreadConfigure` | CNode slot, AddressSpace slot, entry, stack pointer | bind the thread to that capability space and address space, and set where it starts in ring 3 |
| `ThreadResume` | none | make it runnable |
| `ThreadSuspend` | none | stop it |

All three need `WRITE` on the Thread.

**Stopping** a thread removes it from the run queue, or from the endpoint or
Notification queue it waits on: the kernel records which object a thread is
blocked on, and the IPC engine gains `cancel`. A caller waiting in the
stopped thread's reply slot wakes with `PeerExited`. A stopped thread that
was itself waiting for a reply leaves the reply slot it was in, and the
replier's `Reply` then fails with `NoReplySlot`.

A thread stops when it is suspended, when its Thread object, CNode or
AddressSpace is destroyed, or when it faults. A thread that destroys its own
Thread object is stopped on its way out of the kernel.

*As built:*
- **`ThreadResume` always starts the thread at its configured entry.** A
  suspended or exited thread starts over; it does not continue. That is all
  starting a server needs, and it leaves no half-run kernel state to resume.
- **The IPC engine's cancel is `Ipc::stop`.** The kernel's record of what a
  thread is queued on is cleared whenever it is woken, so it never names a
  destroyed object.
- **A running thread that is stopped** switches away when it leaves the
  syscall. If its Thread object was destroyed, the object's slot is kept, and
  its stack stays mapped, until another thread runs. The slot matches no
  object meanwhile, so the memory can be retyped again at once.
- **The root task's own AddressSpace capability** (step 5) came with this
  step, so the boot check can run a thread in the root task's own spaces. Its
  original sits in the root task's keeper CNode. A reap revokes the originals
  before it tears down the address space the loader built, so by then that
  space holds only the loader's own pages.

### 5. The root task starts a task

The root task receives one more capability, its own **AddressSpace**
(delivered with step 4), so it can map frames it retypes into its own space
and write a program into them. The end-to-end boot check:
- The root task builds a child: a CNode, an AddressSpace with its tables, a
  code frame and a stack frame, and a Thread. It gives the child a badged
  console capability, and starts it.
- The child prints through the console.
- The root task then revokes the child Untyped it used. The child stops, and
  all of that memory can be retyped again.

*As built:* the root task cannot mint a console capability with another
badge (its own is badged already), so the child proves it ran by signalling
a Notification of the root task's, then waits on another for good. Its
program is a few hand-assembled instructions that the root task writes into
a frame (`sys::debug::write_bytes`) and maps read and execute in the child's
space. After the revoke, the Notification the child waited on keeps its
bits, the child never runs again, and a Thread can be retyped from the same
memory.

## Consequences

- A task can build another task entirely from its own memory, and revoke
  takes the whole thing back. This is what M5's servers stand on.
- `PageTable` is a new object kind (ABI kind code 7). `MissingTable` and
  `AlreadyMapped` are new error codes.
- **The badge word is overloaded** for Frame and PageTable capabilities. The
  pure crate gives it typed accessors (`Capability::mapping`), so no caller
  packs bits by hand. `CNodeMint` refuses those kinds, so a user cannot forge
  a mapping record.
- **Frame and AddressSpace capabilities may now leave a task**, which ADR
  0020 deferred to this ADR. Revoking a frame that another task mapped
  unmaps it there.
- More kernel tables with fixed bounds: 64 ASIDs, 64 thread slots, 256 slots
  per user CNode. Each bound refuses with `OutOfMemory` rather than growing.
- **Revisit with SMP:** TLB shootdown on unmap, per-ASID locking, and
  stopping a thread that is running on another core.
