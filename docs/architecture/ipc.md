# Inter-process communication

Synchronous, capability-addressed IPC over **Endpoints**: how threads exchange
messages and capabilities, and how a ring-3 task reaches it through the
syscall boundary. Decided in [ADR 0018](../adr/0018-synchronous-ipc.md); the
register ABI is in [syscalls.md](syscalls.md), scheduling in
[scheduling.md](scheduling.md).

## Model

An Endpoint is a rendezvous point with no buffer. A sender and a receiver meet
at it: whichever arrives first blocks, and the other completes the exchange
when it arrives. Threads address an endpoint only through an Endpoint
capability in their own capability space, and its rights decide what they may
do:

| Operation | Rights on the endpoint capability | Effect |
|---|---|---|
| `Send` | `SEND` | deliver a message; block until a receiver takes it |
| `Recv` | `RECV` | block until a message arrives; learn the sender's badge |
| `Call` | `SEND` | send, then block for the reply, as one step |
| `Reply` | none (uses the reply slot) | answer the thread whose `Call` this thread took |
| `ReplyRecv` | `RECV` | reply, then receive: a server's loop in one syscall |

All of them block; there are no non-blocking variants yet.

## Messages and badges

A message is a `MessageInfo` (a label, how many message registers it uses, and
whether it carries a capability) plus up to four message registers. The kernel
copies them from sender to receiver at the rendezvous; nothing goes through
memory. A receiver learns the **badge** of the capability the sender used (0
if unbadged): a server mints one badged `SEND` copy per client and so knows
who is calling, and a client cannot forge another's badge.

## Replies

When a receiver takes a message sent with `Call`, the kernel records the
caller in the receiver's one **reply slot**, and the caller waits for the
reply. `Reply` answers it and empties the slot. Errors land on the thread at
fault: replying with an empty slot, or a plain `Recv` with a caller still in
the slot, is the server's error; a client's `Call` never fails because a
server is busy. If the thread a caller waits on exits, the caller wakes with
`PeerExited`.

## Capability transfer

A message may carry **one** capability, **copied**: the sender keeps its own,
and the receiver gets the same rights and badge. The copy is a child of the
sender's capability in the derivation tree
([capabilities.md](capabilities.md)), so revoking the sender's takes it back.

- The sender's endpoint capability must hold **`GRANT`**; a `Reply` carries
  one only if the caller consented, by naming a receive slot in its `Call`.
- The receiver names one **empty slot** for it when it receives.
- Every check runs before anything moves, and again at the rendezvous; a
  transfer that cannot complete fails the **sender** and delivers nothing, so
  both capability spaces stay unchanged.

Today no capability with `GRANT` leaves a task: the root task's console
capability is `SEND` only. When one does, reaping the granting task takes
back what it granted (ADR 0020): the reap revokes the task's originals, and
a thread blocked on a destroyed endpoint wakes with `Revoked`.

## Waiting and scheduling

Each endpoint holds one FIFO queue of blocked threads, all senders or all
receivers, linked through per-thread IPC state, so the kernel allocates
nothing. A blocked thread is off the run queue; at a rendezvous the waiting
thread becomes ready at the back and the arriving one continues.

## Where the code lives

| Piece | Where |
|---|---|
| Matching, queueing, reply slots, transfer checks | `libs/ipc`, pure and host-tested |
| Endpoint state in memory | `hal::object` (`load_endpoint`, `store_endpoint`), through the HHDM |
| Locking, waking, switching threads | `kernel/src/ipc.rs` |
| Ring-3 entry and decoding | `kernel/src/syscall.rs`, using `capability::abi` |
| User library | `libs/sys` (`send`, `call`, `recv`, `reply`, `reply_recv`, `write`) |

Every IPC operation runs with interrupts off and takes its locks in the fixed
order `IPC`, `CSPACES`, `SCHED`; on one core that makes it atomic with respect
to preemption.

## The console server

The first real IPC client and server. A kernel thread receives on the console
endpoint; each task's capability is badged, and the server buffers a line per
badge and logs it as `console[badge]:`, escaping control characters so a task
cannot forge a kernel log line. The root task prints only this way: the kernel
never prints on a task's behalf.

## What every boot checks

- A server and a client kernel thread: a badged `Call` carrying a `Frame`
  capability, the reply, a send without `GRANT` failing with `NoGrant`, and a
  send through an empty slot failing with `InvalidCap`.
- The root task in ring 3 prints through the console server, and is refused
  when it tries to send its capability onward.
