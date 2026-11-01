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
| 2 | `Recv` | receive from an endpoint; blocks until a message arrives |
| 3 | `Call` | send and wait for the reply; on a non-endpoint capability, invoke a kernel object method |
| 4 | `Reply` | answer the caller in this thread's reply slot |
| 5 | `ReplyRecv` | reply, then receive, in one step |
| 6 | `Yield` | give up the processor |
| 7 | `Exit` | end the calling thread |

Any other number returns `InvalidSyscall`. IPC semantics (rendezvous, badges,
the reply slot, capability transfer) are in
[ADR 0018](../adr/0018-synchronous-ipc.md).

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
| 6 | `UntypedRetype` | Untyped (`RETYPE`) | `mr0` kind, `mr1` count (1), `mr2` dst | none |
| 7 | `NotificationSignal` | Notification (`SEND`) | `mr0` bits | none |

- A CNode method acts on the caller's own capability space, and only through
  a capability to the caller's own CNode.
- A retype creates one Frame, Endpoint or Notification.
- Kind codes: Untyped 0, CNode 1, Frame 2, Endpoint 3, Notification 4,
  AddressSpace 5, Thread 6.
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
| 10 | `WrongObject` | | | |

## The root task

The first user program, `user/root`, is built separately and loaded from a
Limine boot module. It starts with a 64-slot CNode holding exactly:

| Slot | Capability | Rights |
|---|---|---|
| 0 | empty, always | |
| 1 | its own CNode | `READ`, `WRITE` |
| 2 | 64 KiB of Untyped memory | `RETYPE` |
| 3 | the console endpoint, badge 1 | `SEND` only |

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
