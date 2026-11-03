# System calls

The system-call ABI between user programs and the kernel, as decided in
[ADR 0019](../adr/0019-first-user-task.md). The numbers, error codes and method
labels live in one place, `capability::abi`, which the kernel's dispatcher
(`kernel/src/syscall.rs`) and the user library (`libs/sys`) both use.

A task can reach the kernel only through `syscall`, and every syscall names a
capability the task holds. There is no ambient authority: no syscall names an
object by anything but a slot in the caller's own capability space.

## Registers

| Register | In | Out |
|---|---|---|
| `rax` | syscall number | error code (0 = success) |
| `rdi` | capability slot | badge (on receive), else unchanged |
| `rsi` | `MessageInfo` (label, length, capability count) | `MessageInfo` |
| `rdx`, `r8`, `r9`, `r10` | message registers 0 to 3 | message registers 0 to 3 |
| `r12` | receive slot, or `u64::MAX` for none | unchanged |
| `r13` | slot of the capability to send, if `MessageInfo` carries one | unchanged |

`rcx` and `r11` are clobbered by the instruction (return address and flags).
Every other register is preserved, and no kernel value ever reaches a user
register (checked at every boot).

## Numbers

| Number | Name | Meaning |
|---|---|---|
| 1 | `Send` | send through an endpoint; blocks until received |
| 2 | `Recv` | receive from an endpoint; blocks until a message arrives. On a Notification, wait for its bits |
| 3 | `Call` | send and wait for the reply; on a non-endpoint capability, invoke a kernel object method |
| 4 | `Reply` | answer the caller in this thread's reply slot |
| 5 | `ReplyRecv` | reply, then receive, in one step |
| 6 | `Yield` | give up the processor |
| 7 | `Exit` | end the calling thread |

Any other number returns `InvalidSyscall`. IPC semantics (rendezvous, badges,
the reply slot, capability transfer) are in
[ADR 0018](../adr/0018-synchronous-ipc.md).

**`Recv` on a Notification** ([ADR 0022](../adr/0022-notification-wait.md))
needs `RECV` on it. It returns the pending bits in `rdi` and clears them, at
once if any are pending, or else when the next signal arrives; the message
comes back empty, and the receive slot is unused. A signal wakes the longest
waiter, and destroying the Notification wakes every waiter with `Revoked`.
`sys::wait` wraps it.

## Kernel object methods

A `Call` on a capability that is not an endpoint invokes a method on its
object. The label names the method; the message registers carry its
arguments. The reply is `MessageInfo` with label 0 and length 4, badge 0, and
the result in the message registers.

| Label | Method | Target | Arguments | Result |
|---|---|---|---|---|
| 1 | `CNodeCopy` | CNode (`WRITE`) | `mr0` src, `mr1` dst, `mr2` rights | none |
| 2 | `CNodeMint` | CNode (`WRITE`) | as copy, `mr3` badge | none |
| 3 | `CNodeMove` | CNode (`WRITE`) | `mr0` src, `mr1` dst | none |
| 4 | `CNodeDelete` | CNode (`WRITE`) | `mr0` slot | none |
| 5 | `FrameIdentify` | Frame (`READ`) | none | `mr0` kind, `mr1` size |
| 6 | `UntypedRetype` | Untyped (`RETYPE`) | `mr0` kind, `mr1` count (1), `mr2` dst, `mr3` slots (a CNode) | none |
| 7 | `NotificationSignal` | Notification (`SEND`) | `mr0` bits | none |
| 8 | `CNodeRevoke` | CNode (`WRITE`) | `mr0` slot | none |
| 9 | `UntypedSplit` | Untyped (`RETYPE`) | `mr0` size in bytes (whole pages), `mr1` dst | none |
| 10 | `AddressSpaceMapTable` | AddressSpace (`MAP`) | `mr0` PageTable slot, `mr1` vaddr | none |
| 11 | `AddressSpaceMapFrame` | AddressSpace (`MAP`) | `mr0` Frame slot, `mr1` vaddr, `mr2` rights | none |
| 12 | `FrameUnmap` | Frame (`MAP`) | none | none |
| 13 | `ThreadConfigure` | Thread (`WRITE`) | `mr0` CNode slot, `mr1` AddressSpace slot, `mr2` entry, `mr3` stack pointer | none |
| 14 | `ThreadResume` | Thread (`WRITE`) | none | none |
| 15 | `ThreadSuspend` | Thread (`WRITE`) | none | none |

- A CNode method works on the CNode the invoked capability names: a
  destination slot, and the slot a delete or revoke names, are there, while a
  source slot is in the caller's own space. Through a capability to its own
  CNode a task manages its own slots; through one to another CNode it fills
  or clears that one ([ADR 0023](../adr/0023-user-threads-and-address-spaces.md)).
- A retype creates one Frame, Endpoint, Notification, PageTable,
  AddressSpace or Thread, or one CNode, whose slot count (a power of two, at
  most 256) is `mr3`. An AddressSpace starts with the kernel half and an ASID
  (64 at most). A Thread's 16 KiB become the kernel stack of one of 43 thread
  slots.
- **Mapping** ([ADR 0023](../adr/0023-user-threads-and-address-spaces.md)):
  `AddressSpaceMapTable` installs a PageTable at the first missing level
  above `vaddr` (`AlreadyMapped` when all three are there).
  `AddressSpaceMapFrame` maps a 4 KiB frame at a page-aligned user address
  once every level is installed (`MissingTable` otherwise). Its rights are a
  subset of the Frame capability's, with `READ`, and never `WRITE` and
  `EXECUTE` together (`CapNotPermitted`). A capability maps once
  (`AlreadyMapped`); to map a frame twice, copy the capability, and the copy
  starts unmapped. Deleting a Frame or PageTable capability undoes its
  mapping. PageTable capabilities cannot be copied, and Frame and PageTable
  capabilities cannot be minted.
- **Threads** (ADR 0023): `ThreadConfigure` binds a stopped Thread to a CNode
  as its capability space and an AddressSpace, with a user entry and stack
  pointer. `ThreadResume` starts it at that entry, even if it ran before.
  `ThreadSuspend` stops it wherever it is, taking it off any queue it waits
  on. Destroying its Thread, CNode or AddressSpace stops it too; a thread
  that destroys its own never returns to ring 3.
- `UntypedSplit` carves a child Untyped of whole pages from the region and
  installs its capability, derived from the parent's. Its first 64 bytes hold
  its control block, so a page-aligned object retyped from it starts on its
  second page.
- `CNodeRevoke` deletes every capability derived from the one in the slot,
  keeping that one. It makes at most 64 deletions per call, then fails with
  `Again` if more remain: call it again to continue (`sys::revoke` loops).
- Kind codes: Untyped 0, CNode 1, Frame 2, Endpoint 3, Notification 4,
  AddressSpace 5, Thread 6, PageTable 7.
- **No physical address ever reaches ring 3.** A retype answers only success,
  and identifying a frame gives its kind and size.

## Slots

- **Slot 0 is null.** The kernel refuses it wherever a syscall names a slot
  (the invoked capability, a receive slot, the capability to send, a method's
  source or destination), and slot ids wider than 32 bits, with `InvalidCap`.
- **The invoked capability** must be present and of the right kind for the
  operation, or the syscall fails with `InvalidCap`. Slots a method names keep
  finer errors (`CapEmpty`, `CapOccupied`, ...).

## Error codes

| Code | Error | | Code | Error |
|---|---|---|---|---|
| 1 | `MissingRight` | | 11 | `InsufficientRights` |
| 2 | `NoGrant` | | 12 | `CapOutOfRange` |
| 3 | `TooManyCaps` | | 13 | `CapEmpty` |
| 4 | `SourceEmpty` | | 14 | `CapOccupied` |
| 5 | `NoReceiveSlot` | | 15 | `CapNotPermitted` |
| 6 | `ReceiveSlotFull` | | 16 | `OutOfMemory` |
| 7 | `NoReplySlot` | | 17 | `Unsupported` |
| 8 | `ReplySlotFull` | | 18 | `InvalidCap` |
| 9 | `PeerExited` | | 19 | `InvalidSyscall` |
| 10 | `WrongObject` | | 20 | `Again` |
| | | | 21 | `Revoked` |
| | | | 22 | `MissingTable` |
| | | | 23 | `AlreadyMapped` |

`Revoked` wakes a thread blocked on an endpoint that was destroyed, because
its last capability was deleted or revoked.

## The root task

The first user program, `user/root`, is built separately and loaded from a
Limine boot module. It starts with a 64-slot CNode holding exactly:

| Slot | Capability | Rights |
|---|---|---|
| 0 | empty, always | |
| 1 | its own CNode | `READ`, `WRITE` |
| 2 | 128 KiB of Untyped memory | `RETYPE` |
| 3 | the console endpoint, badge 1 | `SEND` only |
| 5 | its own address space | `MAP`, `READ` |

It prints through the **console server**, a kernel thread that receives on the
console endpoint: each `Call` carries up to 32 bytes (label = byte count,
clamped to 32; the message registers hold the bytes little-endian). The server
buffers a line per badge, logs it as `console[<badge>]: ...` at each newline,
and replaces bytes that are not UTF-8 with U+FFFD.

At every boot the root task retypes and signals a Notification, is refused
slot 0, an empty slot and a slot past its CNode (`InvalidCap`), is refused
sending a capability through the console (`NoGrant`), sends malformed console
messages, and exits. A second program, `user/fault`, reads kernel memory and
is ended by its page fault. The kernel then reclaims every frame both used.

## Writing a user program

A program is a `#![no_std]`, `#![no_main]`, `#![forbid(unsafe_code)]` binary
that depends on `libs/sys` and names its entry with `sys::entry!(main)`.
`cargo xtask build` links it with `user/link.ld`: a static executable at
`0x400000`, with text, read-only data and writable data in separate segments,
so the loader maps each with exactly its permissions and never writable and
executable at once. The kernel validates the file with `libs/elf` before
mapping anything.
