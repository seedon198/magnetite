# Threads and scheduling

How the kernel runs more than one thing: threads, the context switch, the
round-robin scheduler, timer preemption, and how a thread ends. Decided in
[ADR 0017](../adr/0017-threads-and-scheduling.md) (threads and scheduling),
[ADR 0019](../adr/0019-first-user-task.md) (user threads) and
[ADR 0021](../adr/0021-fpu-sse-state.md) (FPU and SSE state). The IPC that
blocks threads is in [ipc.md](ipc.md); the syscall ABI in
[syscalls.md](syscalls.md).

## Pieces

| Piece | Where | Role |
|---|---|---|
| Run queue | `libs/sched` (pure, host-tested) | `Scheduler`: a FIFO of ready `ThreadId`s; `admit`, `remove`, `rotate`, `current` |
| Context switch | `hal::context` | `Context` holds a saved kernel `rsp`; `switch` saves callee-saved registers and swaps stacks |
| Thread table | `kernel/src/thread.rs` | per-thread kernel stack, address space, FPU area, exit flag; `switch_to`, `reap` |
| Timer | `hal::lapic` | the LAPIC timer, calibrated against the PIT, fires vector `0x30` at 100 Hz |
| Policy glue | `kernel/src/main.rs` | `pick_next`, `yield_now`, `timer_interrupt`, `exit_current` |

Threads are identified by a `ThreadId`, an index into fixed tables sized by
`thread::MAX`. A `Thread` *capability* object, so a task can create threads
itself, is M5 work (#162).

## A thread's lifetime

1. **Created**: the kernel gives it a kernel stack (from the kernel-stack
   window: one unmapped guard page, then 16 KiB), optionally an address space
   of its own, and a `Context` initialized to start at an entry function with
   interrupts enabled. `SCHED.admit` makes it ready.
2. **Runs**: when `rotate` reaches it. A user thread's entry function drops to
   ring 3 for good (`hal::syscall::enter_user_once`) and comes back to the
   kernel only through syscalls, interrupts and exceptions, each landing on its
   own kernel stack.
3. **Blocks** (IPC only): it leaves the run queue until a rendezvous wakes it
   ([ipc.md](ipc.md)).
4. **Exits**: by the `exit` syscall, or by being killed for a fault in ring 3.
   `exit_current` wakes any caller waiting on its reply, removes it from the
   run queue, marks it exited and switches away for good.
5. **Reaped**: `thread::reap` frees its address space, its kernel stack and
   its slot's state (debug-syscall bit, FPU area), so the id can be reused.
   Reaping a task also reclaims its capability-backed memory, if nothing still
   names it ([capabilities.md](capabilities.md)).

## The context switch

`thread::switch_to(cur, next)` runs with interrupts off and holds no lock
across the switch itself:

1. Points `TSS.rsp0` and the syscall entry's stack at `next`'s kernel stack, so
   the next entry from ring 3 lands on the right one.
2. Loads `next`'s address space into `CR3` if it differs. `AddressSpace::switch`
   first checks the new space maps the running code, the stack and its tables.
3. Saves `cur`'s FPU and SSE registers (`fxsave`) and restores `next`'s
   (`fxrstor`, or a clean default for a thread that never ran).
4. `hal::context::switch` pushes the callee-saved registers on `cur`'s stack,
   stores `rsp` in `cur`'s `Context`, loads `next`'s, pops and returns into it.

A fresh thread's `Context` holds a hand-built frame whose return address is a
trampoline that enables interrupts and jumps to the entry function.

## Scheduling policy

Round-robin over one run queue. `pick_next` rotates the queue: the current
thread goes to the back and the new front runs. It is reached three ways:

- **Cooperatively**: `yield_now`, from kernel code or the `yield` syscall.
- **On a timer tick**: `timer_interrupt` (vector `0x30`, 100 Hz) sends EOI and
  then rotates, preempting kernel threads and ring 3 alike, so one 10 ms time
  slice per turn.
- **On blocking or exit**: the thread leaves the queue and the next one runs.

If the LAPIC timer cannot be calibrated (for example, no PIT), the kernel warns
and keeps **cooperative** scheduling (#109); CI boots that case too.

**The idle thread.** There is no separate one: the boot thread is it. It never
blocks or exits; once boot is done it waits in `hlt` with interrupts on, so
the run queue is never empty and an otherwise idle CPU halts until the next
interrupt. Blocking asserts this.

## Concurrency rules

Single core. Every lock is a `SpinLock` that disables interrupts while held,
and locks nest in one order: `IPC`, `CSPACES`, `SCHED`, the thread table's
`SPACES`, `KERNEL_SPACE`, then the frame allocator. No lock is held across a
context switch. A preempted thread restores its own interrupt flag when it is
resumed (from the trap frame, or on return from its own `switch_to` caller).

## What every boot checks

- Two kernel threads alternate cooperatively; two spinners that never yield
  are time-sliced by the timer.
- User threads block inside syscalls and come back on their own stacks, are
  preempted in ring 3, keep their own SSE registers across switches, and every
  frame they used comes back when they are reaped.
- A boot-time wait for a task gives up after a bounded number of yields and
  panics with the task's name, rather than hanging (#136).
