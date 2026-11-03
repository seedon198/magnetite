# 0022: Notification wait: `Recv` on a Notification capability

- **Status:** Accepted
- **Date:** 2026-11-04

## Context

Issue #162 (M5): a Notification can be signalled, from the kernel or from
ring 3 (`NotificationSignal`), but nothing can wait on one.
`NotificationState::waiters` has been reserved for this since M3. User servers
need it: a driver waits for its interrupt, and a server waits for "work is
ready" without a blocking IPC partner.

What exists, and limits this design:

- Blocking already has one shape, in the pure `ipc` engine: a thread's state
  and its queue link live in the per-thread IPC state, and an object stores
  only the ends of its queue, so the kernel allocates nothing
  ([ADR 0018](0018-synchronous-ipc.md)).
- The syscall ABI has `Recv` (blocks on a capability, returns a badge in `rdi`
  and a message) and a method call (`Call` on a non-endpoint capability,
  which never blocks) ([ADR 0019](0019-first-user-task.md)).
- Destroying an object wakes the threads blocked on it with `Revoked`
  ([ADR 0020](0020-capability-derivation-and-revoke.md)).

## Options considered

1. **`Recv` on a Notification capability** (seL4's `Wait`): no new syscall
   number. The pending bits come back in the badge register.
2. **A `NotificationWait` method label** under `Call`: methods would then
   block, which none does today, and a `Call` that blocks for something other
   than a reply is a new shape for the syscall path.
3. **A new syscall number, `WAIT`**: clearer by name, but it duplicates
   `Recv`'s register contract for no gain.

## Decision

Option 1.

- **`Recv` on a Notification capability** needs `RECV` on it (`MissingRight`
  otherwise). If any bit is pending, it returns at once. Otherwise the thread
  blocks until a signal arrives. Either way it returns the pending word in
  `rdi` (the badge register) and clears it, consuming every pending bit at
  once. The message info and registers come back as zero, and the receive
  slot is unused: nothing but bits can arrive.
- **`NotificationSignal`** ORs its bits into the word. If a thread is
  waiting, the longest waiter wakes with the whole word, and the word is
  cleared again. One signal wakes one waiter. A signal of no bits wakes no
  one.
- **Destroy**: when the last capability to a Notification is deleted, every
  waiter wakes with `Revoked`, as for an Endpoint.
- **Layout**: `NotificationState` grows a third word, so the queue keeps both
  ends (`waiters` head, `waiters_tail`), encoded like an endpoint's.
- **Where the code lives**: the pure `ipc` engine owns waiting, signalling
  and destroy (`Ipc::wait`, `Ipc::signal`, `Ipc::destroy_notification`), so
  they are host-tested; the kernel loads and stores the state through
  `hal::object` and applies the woken threads to the scheduler. A signal
  therefore runs under the `IPC` lock, as every invocation already does.
- `libs/sys` offers `wait(slot) -> Result<u64, SysError>`.

## Consequences

- A thread can block without an IPC partner. This is the building block for
  interrupt delivery to user drivers later in M5.
- Signals merge: bits signalled while no one waits are kept and delivered
  together, and their count is lost. A server that needs counts must keep
  them in shared memory.
- A Notification is now 24 bytes instead of 16; retyping places it the same
  way.
- The waiter queue has no priority: waiters are served in arrival order,
  like an endpoint's.
