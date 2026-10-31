# 0018: Synchronous IPC over Endpoints

- **Status:** Accepted
- **Date:** 2026-11-01

## Context

IPC is how a microkernel's isolated tasks talk. A client asks a server for
work, and authority moves between tasks by sending capabilities. Until IPC
exists, capabilities have no way to travel and servers (M5) cannot be built.
[ADR 0016](0016-capability-implementation.md) deferred blocking IPC to M4,
because a thread must be able to block. [ADR 0017](0017-threads-and-scheduling.md)
put it after threads, ring 3 and preemption, all now merged (#78, #79, #80).

What exists today, and constrains this design:

- `Endpoint` objects can be retyped from Untyped. `EndpointState` is a zeroed
  `repr(C)` struct with a `queue_head` and three reserved words; nothing uses
  it yet.
- The message ABI (`capability::abi`) packs a `MessageInfo` word (label,
  length, caps count) and carries up to `MSG_REGISTERS = 4` message registers.
- Capabilities already carry a badge; `CNodeMint` sets one.
- The scheduler (`libs/sched`) is a round-robin ready queue keyed by
  `ThreadId`. It has no blocked state.
- Threads are a fixed array of saved contexts in `kmain` (boot thread, demo
  workers, preemption spinners), not yet `Thread` objects retyped from Untyped.
- Every ring-3 thread shares one kernel stack (`TSS.rsp0`), so a user thread
  cannot yet block inside a syscall. Per-thread kernel stacks are #82.
- Every `SpinLock` holder runs with interrupts off, and nothing switches
  threads while holding a lock (the deadlock rules from #80, in
  [interrupts.md](../architecture/interrupts.md)).

Security forces: IPC is the only channel between tasks, so it is where
authority can leak. A capability must never move without the sender's explicit
right to give it away, a reply must reach exactly the caller once, and a
receiver must control where incoming capabilities land
([threat model](../architecture/threat-model.md)).

## Options considered

**Scope of the first IPC milestone (#81).**

1. **IPC engine verified between kernel threads; user mode in #82.** Keeps the
   PR reviewable and tests the logic before ring 3 is involved. #82 then adds
   per-thread kernel stacks and `Thread` objects and routes the syscall path
   into the same engine.
2. **Straight to user space.** One large PR carrying the engine, per-thread
   kernel stacks, `Thread` objects and the syscall wiring.

**Replies.**

1. **Implicit one-shot reply slot (classic seL4).** `Call` leaves a reply right
   in the server's thread state, and `Reply` consumes it. Unforgeable,
   uncopyable, single use. A server holds one pending reply at a time.
2. **Explicit Reply objects (seL4 MCS).** The server passes a Reply object to
   `Recv`. Many outstanding replies, but every server manages extra objects.

**A capability transfer that cannot complete** (sender lacks GRANT, or the
receiver named no slot or a full one).

1. **Fail the send with an error; deliver nothing.** Mistakes surface on first
   use. The sender can learn that the receiver had no free slot ready, a small
   leak we accept.
2. **Deliver the message without the capability (seL4).** The receiver sees zero
   caps; the sender learns nothing, but bugs surface late and far from the cause.

## Decision

**Scope: option 1.** #81 builds the IPC engine and proves it between kernel
threads in QEMU. #82 adds per-thread kernel stacks and `Thread` objects and
connects ring 3 through the syscall boundary to the same engine.

**Operations.** All blocking:

| Operation | Needs on the endpoint cap | Effect |
|---|---|---|
| `Send` | `SEND` | Deliver a message; block until a receiver takes it. |
| `Recv` | `RECV` | Block until a message arrives; learn the sender's badge. |
| `Call` | `SEND` | Send, then block for the reply as one atomic step. |
| `Reply` | none (uses the reply slot) | Answer the thread whose `Call` this thread received. |
| `ReplyRecv` | `RECV` | `Reply`, then `Recv` in one operation: the server loop. |

Non-blocking variants (try-send, poll) are left for later.

**Replies: option 1, the implicit reply slot.** When a receiver takes a
message sent with `Call`, the kernel records the caller in the receiver's
reply slot, and the caller blocks awaiting a reply. `Reply` delivers a
message to that caller, makes it ready and empties the slot. A reply slot
holds at most one caller. Saving a reply into a capability slot, for
servers juggling several clients, is a later addition. The edge cases are
the builder check's call, made to keep every error on the thread at fault:

- **`Reply` with an empty slot** is an error to the replier.
- **Plain `Recv` with a full reply slot** is an error to the server, at once.
  Only the server is at fault, so clients never see a `Call` fail because the
  server is busy, and learn nothing of its state.
- **`ReplyRecv` with an empty slot** skips the reply half and just receives,
  so a server loop's first iteration works.
- **A failed reply** (for example, its capability transfer cannot complete)
  returns the error to the server; nothing is delivered, the slot keeps the
  caller, the caller stays blocked, and for `ReplyRecv` the receive half does
  not run. The server can retry.

**Messages.** A message is a `MessageInfo` label plus up to 4 message
registers, the existing ABI. The kernel copies the registers from sender to
receiver at the rendezvous. A per-thread buffer page for longer messages
comes when the first server needs it (M5).

**Badges.** The receiver learns the badge of the capability the sender used
to invoke the endpoint (0 if unbadged), so a server can tell its clients apart.

**Capability transfer.**

- At most **one capability per message** (`MessageInfo::caps` is 0 or 1;
  anything larger is an error).
- The sender names a slot in its own cap space; the capability is **copied**
  (the sender keeps it) with the **same rights and badge** the sender holds. To
  hand over less, the sender first derives a weaker copy (`CNodeCopy`).
- The sender's endpoint capability must hold **`GRANT`**.
- The receiver names, when it calls `Recv` (or `ReplyRecv`), **one empty slot**
  in its own cap space for an incoming capability.
- **Option 1: a transfer that cannot complete fails the send** with a distinct
  error (no `GRANT`; no receive slot named; receive slot full; source slot
  empty), and nothing is delivered: neither message nor capability. Every
  check happens before anything moves, so a failure leaves both cap spaces
  unchanged.
- **When the checks run** (the builder check's call). The sender's own checks
  (`GRANT`, caps count, a non-empty source slot) run when it invokes, so its
  own mistakes fail at once without blocking. A receiver naming a slot that is
  already full gets an error from `Recv` at once. Everything is checked again
  at the rendezvous, since another thread may have emptied the source slot or
  filled the receive slot meanwhile.
- **Who gets the error at the rendezvous** (the builder check's call). The
  **sender always does**, whichever side arrived first: an arriving sender
  returns the error without blocking; a queued sender is dequeued and woken
  with it. The receiver is never penalized: a waiting receiver stays blocked,
  and an arriving receiver moves on to the next queued sender, or blocks.
  Matching stays strictly FIFO: a capability-carrying sender meeting a
  receiver that named no slot fails, even if a later receiver could have
  taken it.
- **A `Reply` may carry one capability, with the caller's consent.** A reply
  uses no endpoint capability, so `GRANT` cannot apply. Instead the caller
  consents by naming an empty receive slot in its `Call`. If it named one, the
  server may reply with a capability into it. If it named none, a reply
  carrying a capability fails (a failed reply, above). The other checks are as
  for a send (source slot non-empty, receive slot still empty). We chose this
  over seL4's rule, which requires `GRANT` on the caller's endpoint capability:
  consent stays per call and with the client, and clients never need `GRANT`,
  which would also let them push capabilities into servers.

**Waiting.** Each endpoint holds one FIFO queue of blocked threads plus a tag
saying whether they are senders or receivers. It can never hold both at once:
a sender arriving while receivers wait rendezvouses immediately with the
first of them, and vice versa. The queue lives in `EndpointState`, linked
through per-thread IPC state, so the kernel allocates nothing. `EndpointState`
keeps its size; the reserved words become the tail and the tag.

**Scheduling.** The scheduler gains a blocked state: a blocked thread is out of
the ready queue until IPC wakes it. At a rendezvous the thread that was
waiting becomes ready (at the back of the queue) and the arriving thread
continues. A direct switch to the partner (seL4's fast path) is a later
optimization. If every thread is blocked, the CPU idles in `hlt` until the
timer, as today.

**Concurrency.** Every IPC operation runs with interrupts off and takes the
locks it needs in one fixed order, never switching threads while holding one
(the #80 rules). On this single core, that makes each operation atomic with
respect to preemption.

**Where the code lives.** The queueing and matching logic, per-thread IPC
state (blocked-on, reply slot, receive slot, message registers, queue link)
and the transfer checks go in a pure `#![forbid(unsafe_code)]`, host-tested
crate (`libs/ipc`, or a module of `capability` if it stays small; the
builder decides). It is generic over the thread id and cap-space type, like
`capability` and `sched`. The kernel moves registers, applies the result
(block, wake, copy capability) and switches threads. Reading and writing
`EndpointState` through the HHDM goes through `hal::object`, like
Notifications today. Until `Thread` objects arrive in #82, the kernel keeps
per-thread IPC state in a fixed array indexed by `ThreadId`, beside
`CONTEXTS`.

**Verification (the "done when" of #81).** In QEMU, with the boot banner's
log:

1. A server kernel thread `Recv`s on an endpoint, holding `RECV`. A client
   kernel thread holds a **badged** `SEND | GRANT` copy, and `Call`s with a
   label, message registers and one capability (for example a `Frame`). The
   server logs the label, the registers, the badge and the received
   capability's kind and rights, then `Reply`s with a message.
2. The client logs the reply.
3. A send that carries a capability through a cap **without `GRANT`** fails
   with the no-grant error, and neither side's state changes.
4. A plain `Send` without a capability through that same no-`GRANT` cap
   succeeds.

The pure crate's host tests cover the matching rules: queue order, every
transfer error, the reply-slot lifecycle, and the senders/receivers tag.

## Consequences

- Capabilities can now move between threads, which every M5 server depends on.
  Moving them requires an explicit right (`GRANT`), and they can only be
  attenuated, never amplified.
- The logic that decides who meets whom and what moves is host-tested; only the
  register copy and the switch live in the kernel.
- #82 inherits a tested engine: it adds per-thread kernel stacks, `Thread`
  objects and the syscall decoding, not IPC semantics.
- A server holds one pending reply at a time until a save-reply operation is
  added. Messages are limited to 4 registers until the buffer page lands.
- Failing a send on an unusable receive slot tells the sender the receiver was
  not ready. We accept that small leak for errors that are visible at once.
- Revisit: the fast-path direct switch, non-blocking variants, save-reply,
  multi-capability messages, and the long-message buffer, each when a real
  server needs it.
