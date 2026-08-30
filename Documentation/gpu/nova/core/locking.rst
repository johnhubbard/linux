.. SPDX-License-Identifier: GPL-2.0
.. SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

============================================
Locking: GPU access state and the GSP queues
============================================

This document describes how nova-core serializes access to shared state. It
covers the state machine that decides whether the driver may reach the GPU at
all, the locks that protect the GSP command and message queues, what the
interrupt path may touch, the total lock order, and the contract a second-level
driver such as nova-drm can rely on.

nova-core has one lock today, ``inner: Mutex<CmdqInner>`` in ``gsp/cmdq.rs``.
Every other piece of mutable state is either written once during construction or
reachable only through ``&mut self`` while the driver is being built or torn
down. That single lock is held across a whole send-and-receive cycle, so a
5-second RPC blocks the interrupt thread and every other sender behind it. This
document replaces it with locks that each protect one named piece of state.

Terminology
===========

GPU access state
    Which of six states the GPU is in, from the driver's point of view: whether
    the driver may read or write its registers and start DMA at all. The states
    are ``Off``, ``Booting``, ``Ready``, ``Suspending``, ``Resuming``, and
    ``Dead``.

enter, exit
    Take and release a reference on the GPU access state. Entering succeeds only
    in ``Ready``, and while any thread is inside, the state cannot change. DRM
    names the same operation ``drm_dev_enter()`` and ``drm_dev_exit()``.

holder
    A thread that has entered and not yet exited.

transition
    A state change. Exactly one thread owns a transition at a time, and every
    transition except the one to ``Dead`` waits for each holder to exit before it
    completes.

sender
    A thread inside ``Cmdq::send_command``, which writes a command into the
    CPU-to-GSP ring and waits for the reply.

consumer
    The thread that reads the GSP-to-CPU ring and advances the CPU read pointer.
    The GPU access state decides which thread that is, and ``gspq`` covers the one
    window during probe where the outgoing consumer and the incoming one overlap.
    "The receive path" describes both.

queue element
    One entry in either ring, described by a ``GspMsgElement`` header followed by
    the command or message body. An element occupies a whole number of GSP pages
    and is at most ``GSP_MSG_QUEUE_ELEMENT_SIZE_MAX`` (64 KiB) long.

element sequence number
    The ``seqNum`` field of a queue element, which advances once per element,
    including each continuation record. nova-core calls the counter ``elem_seq``.

RPC sequence number
    The ``sequence`` field of the RPC header inside a queue element. The GSP
    echoes it in the reply, which is what lets a reply be matched to the command
    that awaits it. nova-core calls the counter ``rpc_seq``.

in-flight command
    A command whose queue elements have been published to the GSP and whose reply
    has not yet arrived or timed out.

command deadline
    The sum of ``ALLOCATE_TIMEOUT`` and ``RECEIVE_TIMEOUT``, which comes to 6
    seconds for a command that fits one queue element. It bounds how long one
    ``send_command`` call can run, and so how long it can hold whatever it holds.
    A multi-element command adds up to ``ALLOCATE_TIMEOUT`` per further element.

The rest of the interrupt vocabulary (GIN vector, leaf, subtree, rearm, top half,
IRQ thread) is defined in ``Documentation/gpu/nova/core/interrupts.rst``.

What one queue lock costs
=========================

``Cmdq::send_command`` takes ``inner``, writes the command, rings the GSP
doorbell, and then polls the GSP-to-CPU ring for the reply without releasing the
lock. Its own doc comment records the consequence: "The queue is locked for the
entire send+receive cycle, so no other command can be interleaved."
``Cmdq::RECEIVE_TIMEOUT`` is 5 seconds, and the wait for ring space earlier in
the same critical section adds ``CmdqInner::ALLOCATE_TIMEOUT``, 1 second, so the
worst-case hold is the command deadline.

Four costs follow.

A sender is also the receiver and the demultiplexer for every message that
arrives while it waits. ``CmdqInner::receive_msg`` decodes a matching reply and
routes everything else to ``CmdqInner::dispatch_event``, returning ``ERANGE`` so
the caller loops. A thread that sent a short command does the work of
dispatching an unrelated burst of GSP log records.

The IRQ thread blocks behind a sender. ``GspInterrupt::handle_threaded`` calls
``Cmdq::drain``, which takes the same lock, so a GSP notification can wait up to
the command deadline before it is serviced. Log and error records are delayed by
that much, and so is the robust-channel recovery notice that
``CmdqInner::dispatch_event`` logs for ``MsgFunction::RcTriggered``.

Senders do not make progress concurrently even when the GSP could serve them
concurrently. Only one command is outstanding at a time, so the round-trip
latency of every command adds up rather than overlapping.

The wait for space in the CPU-to-GSP ring happens inside the lock.
``DmaGspMem::allocate_command`` polls ``driver_write_area_size`` for up to
``CmdqInner::ALLOCATE_TIMEOUT``, 1 second, and ``read_poll_timeout`` sleeps
between polls. A sender waiting for the GSP to consume ring space holds the lock
that the consumer needs.

The split is possible because the two directions do not share state.
``DmaGspMem`` covers both rings, but the send and the receive path access disjoint
pointer sets:

==================  =====================================  =========================
Path                Writes                                 Reads
==================  =====================================  =========================
send                ``cpuq.tx`` write pointer,             ``gspq.rx`` read pointer
                    ``cpuq.msgq.data``
receive             ``cpuq.rx`` read pointer               ``gspq.tx`` write pointer,
                                                           ``gspq.msgq.data``
==================  =====================================  =========================

Only the single lock couples them.

The GPU access state
====================

Every process-context path that reads a register, writes a register, or starts
DMA passes through one state machine, so that nova-core never touches hardware
while the GPU is powered down, still booting, suspending, resuming, or dead after
a GSP crash. Many threads are inside it at once, and entering it when the GPU is
ready costs one atomic compare-and-exchange. The interrupt top half cannot sleep
and so cannot enter, and "The interrupt path" gives the separate argument that
keeps it safe.

The states
----------

::

                    +---------------------------------------+
                    |                                       |
                    v                                       |
    (probe)  -->  Booting  -->  Ready  -->  Suspending  --> Off
                    |             |  ^                       |
                    |             |  |                       |
                    v             |  +------  Resuming  <----+
                   Off  <---------+            |
                                  |            |
                                  +---> Dead <-+
                                        ^
                                        |
                            corrupt framing, drain failure,
                            or a GSP crash reported by the falcon

``Off``
    No GSP is running. The BAR mapping may or may not exist. Entering fails.
    This is the state before ``Gpu::new`` runs and after the GSP is unloaded.

``Booting``
    The boot sequence in ``gsp/boot.rs`` is running. No interrupt is registered
    and no IRQ thread exists. Ordinary entering fails, and the thread that owns
    the transition may touch hardware through its transition guard.

``Ready``
    The GSP is up, the GSP notification vector is enabled, and the IRQ thread
    exists. Entering succeeds.

``Suspending``
    A power transition has been requested. Entering fails immediately for a
    non-blocking caller and blocks for a blocking one. Existing holders run to
    completion.

``Resuming``
    The GSP is being booted again after a suspend. Entering blocks for a blocking
    caller and fails for a non-blocking one. As in ``Booting``, the transition
    owner may touch hardware.

``Dead``
    The GSP-to-CPU queue cannot be advanced, or the GSP has crashed. Nothing
    short of a device reset recovers. Entering fails permanently, and every
    in-flight command is failed at once rather than being left to time out.

Which primitive, and why
------------------------

The read side has to be able to sleep. A holder waits for a GSP reply for up to
``RECEIVE_TIMEOUT``, and even a holder that only pokes registers sleeps inside
``read_poll_timeout``, which calls ``might_sleep()`` unconditionally and
``fsleep()`` between polls. That requirement eliminates one of the four
candidates outright.

===========================  ==============  ================================  ==================
Primitive                    Read may sleep  State transition                  In-tree today
===========================  ==============  ================================  ==================
rwsem abstraction            yes             ``down_write`` waits for readers   no
SRCU abstraction             yes             ``synchronize_srcu`` waits a        no
                                             grace period
state word plus condvar      yes             store the state, then wait for     yes
                                             the holder count to reach zero
``Revocable``                no              ``synchronize_rcu``, and the       yes
                                             wrapped object is dropped
===========================  ==============  ================================  ==================

``Revocable`` fails two requirements. Its guard holds the RCU read-side lock, so a
holder may not sleep, which its own doc comment states. And ``revoke()`` is
one-way and drops the wrapped object, so it cannot express a suspend followed by a
resume.

The other three all pay the same price on a transition: a transition cannot
complete until the longest-running holder finishes, so a suspend requested while
an RPC is in flight waits up to ``RECEIVE_TIMEOUT``. That cost is inherent to the
requirement rather than to the primitive, so it does not decide the choice.

The deciding difference is that neither an rwsem nor SRCU carries the state. Each
records only "inside" or "outside", so either one needs a separate state variable,
and a transition can then run between the state check and the reference
acquisition. Serializing those two against a transition would take a third lock.
A state word makes them one operation:

::

    word: Atomic<u32>     [ state: 3 bits | holders: 29 bits ]

    try_enter():
        loop:
            w = word.load(Acquire)
            if state(w) != Ready:
                return Err(...)
            if word.cmpxchg(w, w + 1, Acquire).is_ok():
                return Ok(guard)

    exit():
        w = word.fetch_sub(1, Release)
        if holders(w) == 1 and state(w) != Ready:
            changed.notify_all()

The fast path performs one compare-and-exchange, the same cost as an rwsem's
``down_read``. SRCU's per-CPU read side is cheaper, and that is the one real
trade: it matters at millions of acquisitions per second, which nova-core's
surface does not approach. The API is ``enter`` and ``try_enter`` returning a guard, so the
internals can be replaced with SRCU later without touching a call site.

``GpuAccess`` holds:

* ``word: Atomic<u32>``, the state and the holder count, as above.
* ``state_lock: Mutex<()>``, which serializes ``GpuAccess::begin`` against itself
  and backs ``changed``. It is never taken on the entering path.
* ``changed: CondVar``, on which a blocking caller waits out a ``Booting``,
  ``Suspending``, or ``Resuming`` state, and on which a transition owner waits
  for the holder count to reach zero.

29 bits of holder count is 536 million, which no thread count reaches.

The two acquire forms
---------------------

The caller chooses the policy, because the right answer differs between an
interrupt thread and a userspace-driven request.

``GpuAccess::enter()``
    Blocks through ``Booting``, ``Suspending``, and ``Resuming`` until the state
    is ``Ready``, and fails with ``ENODEV`` in ``Dead``. In ``Off`` it fails with
    ``ENODEV`` while no resume path is installed, and starts a resume once one is
    (see the runtime-PM entry under "Extending the model"). A caller that is
    willing to wait out a resume uses this. It may sleep and must not be called
    from atomic context.

``GpuAccess::try_enter()``
    Returns ``EAGAIN`` in ``Booting``, ``Suspending``, and ``Resuming``, and
    ``ENODEV`` in ``Off`` and ``Dead``. A caller that must not add latency to a
    transition uses this.

Both take ``Bar0<'a>`` and return a ``GpuAccessGuard<'a>`` that carries it, so a
function that needs the BAR and the state takes one argument instead of two and
cannot be handed a BAR without a state. ``GpuAccess`` itself stores no BAR, which
is what keeps it constructible in a KUnit test.

BAR validity stays a compile-time property. ``Bar0<'a>`` is
``&'a pci::Bar<'a, BAR0_SIZE>`` and its lifetime is tied to
``pci::Device<Bound>``. The guard carries the compile-time BAR proof and the
runtime state proof side by side, and does not convert the first into the
second.

Which form each call site uses
------------------------------

============================================  ====================================
Call site                                     Form
============================================  ====================================
``NovaCoreDriver::probe``                      creates ``GpuAccess`` in ``Booting``
``Gpu::new`` and everything it calls           the ``Booting`` transition guard
``irq::gsp::quiesce``                          the same transition guard
``GspIrq::new``                                the same transition guard
``Cmdq::drain`` at the end of probe            the same transition guard
``Gsp::get_static_info`` during probe          the same transition guard
``GspInterrupt::handle`` (top half)            none, see "The interrupt path"
``GspInterrupt::handle_threaded``              ``try_enter``
``GspResources::drop`` and ``Gsp::unload``     an ``Off`` transition guard
debugfs reads of ``Gsp::logs``                 none, see the state inventory
future nova-drm entry points                   ``enter``
future ``pm_runtime_get``                      ``enter``
future ``pm_ops`` suspend and resume           a transition guard
============================================  ====================================

The IRQ thread uses ``try_enter`` because waiting through a suspend would delay
the suspend transition. A
notification it declines to service stays latched: the top half has already
cleared the leaf and retriggered the falcon, so the next posted message raises
the interrupt again, and the resume path drains the queue before it publishes
``Ready``.

What is held across what
------------------------

A holder holds the state across a whole logical operation rather than across each
individual access. For an RPC the operation starts before the reply buffer is
allocated and ends when the reply has been decoded or the command deadline has
passed.

Per-access entering would be wrong. A single RPC writes the command body into the
ring, then writes the write pointer, then writes ``NV_PGSP_QUEUE_HEAD``. A
suspend that started between the second and third of those would tear the GSP
down with a command published and unread, and the reply would arrive into a
queue that no longer exists. The unit that must be indivisible is the operation,
so that is the unit the state is held across.

A transition waits, and the wait is bounded. Every holder in nova-core is one of
two kinds: a register sequence measured in microseconds, or one RPC bounded by the
command deadline. A transition waits for the holder count to reach zero with a
timeout of the command deadline plus a margin. If that expires, a holder is stuck,
which is a driver bug, and the transition fails with ``EBUSY``, a legal return
from a PM callback.

In-flight commands are not aborted. A published command may already have been read
and acted on by the GSP, and there is no way to un-send it. The open-source GSP-RM
driver makes the same choice: on an RPC timeout it logs and continues, and marks
the device for reset only after three consecutive timeouts.

Who owns the state machine
--------------------------

``GpuAccess`` owns the state word. A transition is owned by whoever holds the one
``TransitionGuard``, which ``GpuAccess::begin`` hands out and which excludes
every other transition. Only probe, unbind, and the PM callbacks take one.

===============  ==============  =============================================
From             To              Driven by
===============  ==============  =============================================
(construction)   ``Booting``     ``NovaCoreDriver::probe``
``Booting``      ``Ready``       probe, after the boot-time ``Cmdq::drain``
``Booting``      ``Off``         a probe failure, through the partial unwind
``Ready``        ``Suspending``  ``pm_ops.suspend``, or a runtime-PM idle
                                 callback
``Suspending``   ``Off``         the same callback, once the holders have
                                 exited and ``UnloadingGuestDriver`` has been
                                 acknowledged
``Off``          ``Resuming``    ``pm_ops.resume``, or ``pm_runtime_resume``
``Resuming``     ``Ready``       the same callback, after the GSP boots and the
                                 queue is drained
``Resuming``     ``Dead``        a resume failure
``Ready``        ``Off``         unbind, through ``NovaCore``'s drop
any              ``Dead``        corrupt framing on the receive path, or the
                                 drain-failure path in ``handle_threaded``
===============  ==============  =============================================

The transition to ``Dead`` is the one that does not wait for holders. It is a
single store that fails every later entry, followed by a walk of the in-flight
table that completes each waiter with ``EIO``. Existing holders finish on their
own, and they finish with an error rather than a 5-second timeout.

The command queue
=================

``CmdqInner`` splits into ``CpuQueue``, ``GspQueue``, and ``InFlightCommands``,
one per ring direction plus the reply-matching table.

============  ================================================  =========================
Lock          Protects                                          Maximum hold
============  ================================================  =========================
``cpuq``      the ring write pointer, the slots, the sequences  one command's elements
``gspq``      the CPU read pointer into the GSP-to-CPU ring     one message copy
``inflight``  the table of commands awaiting a reply            one table entry
============  ================================================  =========================

None of the three allocates while held. ``gspq`` and ``inflight`` also never
sleep while held, which is why the IRQ thread can take them. A command that fits one queue element is the whole
hold in the common case, and ``cpuq`` does not sleep for one. "Multi-element
commands" covers the exception.

The ``Coherent<GspMem>`` allocation itself stays outside every lock, in ``Cmdq``,
because the allocation is immutable once made. ``CpuQueue`` and ``GspQueue`` hold
the driver-side scalars for their own direction, and each mutex protects that
direction's pointers and slots. ``DmaGspMem``'s
accessors split the same way: ``driver_write_area``, ``driver_write_area_size``,
``allocate_command``, and ``advance_cpu_write_ptr`` become operations on
``CpuQueue``, and ``driver_read_area`` and ``advance_cpu_read_ptr`` become
operations on ``GspQueue``.

``dev`` moves out of the guarded state to ``Cmdq``, where it needs no lock. It is
an ``ARef<device::Device>`` that is written once, and moving it lets any path log
without taking anything. ``GspInterrupt`` already keeps its own ``dev`` for
exactly this reason.

The send path
-------------

::

    enter the GPU access state                     -> GpuAccessGuard
    allocate the reply buffer (GFP_KERNEL)
    loop:
        lock cpuq
            if the ring has room:
                rpc_seq = cpuq.rpc_seq++
                reserve the slots
                write the header, command, and payload
                compute the checksum
                lock inflight
                    insert (rpc_seq, deadline, reply buffer, completion)
                unlock inflight
                elem_seq advances once per element written
                advance the write pointer
                write NV_PGSP_QUEUE_HEAD
                unlock cpuq
                break
            unlock cpuq
        fsleep(1us), until ALLOCATE_TIMEOUT
    wait_for_completion_timeout(RECEIVE_TIMEOUT)
    decode the reply with M::read, outside every lock
    exit the GPU access state

Three constraints fix that order.

The reply buffer is allocated before ``cpuq`` is taken, so no lock is held across
a ``GFP_KERNEL`` allocation.

The in-flight entry is inserted before the write pointer moves. If it were
inserted after, the GSP could reply between the doorbell and the insertion, and
the consumer would find no waiter and drop the reply as stale.

The wait for ring space releases ``cpuq`` between polls. Today's
``allocate_command`` polls inside the lock for up to 1 second. Splitting it into
a check-and-reserve under the lock with the sleep outside turns a 1-second hold
into a sequence of microsecond holds. The check and the reservation stay in one
critical section, so two senders cannot both conclude that the same slot is free.

Multi-element commands
----------------------

``CmdqInner::send_command`` splits a command larger than
``GSP_MSG_QUEUE_ELEMENT_SIZE_MAX`` into a head element followed by continuation
records. A second sender stays out of the middle of that run because ``cpuq`` is
held from the first element to the last, and the write pointer is advanced once
per element with the doorbell rung once per element, all under that one hold. A
second sender cannot reserve a slot in the middle because it cannot take
``cpuq``.

That hold is the exception to ``cpuq`` never sleeping. The release-between-polls
rule applies to the first element only. Once a run has started, a sender that
runs out of ring space must wait for it with ``cpuq`` still held, because
releasing the lock is what would let a second sender in. The wait is bounded by
``ALLOCATE_TIMEOUT`` per element, and the element count is the payload divided by
``GSP_MSG_QUEUE_ELEMENT_SIZE_MAX``, rounded up, plus one for the head. The IRQ
thread is unaffected, because it never takes ``cpuq``.

Both reference drivers hold one lock across the whole run for the same reason.
nouveau takes ``gsp->cmdq.mutex`` in ``r535_gsp_rpc_push`` before the head
element and releases it after the reply, and the open-source GSP-RM driver
asserts the per-GPU lock on every send. The GSP-side source of a later ABI checks
the RPC sequence number of each continuation record against the previous one plus
one, and on a mismatch stashes the interloping head and fails the multi-element
command. Interleaving does not corrupt the firmware, but it does fail the
command.

Sequence numbers
----------------

``elem_seq`` and ``rpc_seq`` are allocated under ``cpuq``, not atomically.

Allocating them atomically and then racing to publish would let two senders take
sequence numbers in one order and reach the ring in the other. Both reference
drivers allocate under the lock that also reserves the slot: nouveau does
``msg->sequence = gsp->cmdq.seq++`` and ``rpc->sequence = gsp->rpc_seq++`` inside
``cmdq.mutex``, and the open-source GSP-RM driver does a plain non-atomic
post-increment on ``pRpc->sequence`` with the per-GPU lock asserted. Neither uses
an atomic, and neither needs to.

Whether the GSP requires the element sequence numbers of successive queue
elements to increase by exactly one is not confirmable from the available source
(see "What is not confirmed"). The design keeps publish order equal to
``elem_seq`` order, which needs no extra synchronization because the slot
reservation already serializes senders.

The receive path
----------------

One consumer reads the GSP-to-CPU ring. The GPU access state decides which
thread that is, so there is no separate mode flag to keep in step:

* In ``Booting`` and ``Resuming`` there is no IRQ thread, so the sending thread
  is the consumer. It sends, then loops taking ``gspq`` and draining until its
  own reply appears or the ring is empty, sleeping between passes.
* In ``Ready`` the IRQ thread is the consumer. A sender waits on its own
  completion and never touches ``gspq``.

The switch happens once. ``Gpu::new`` runs to completion before
``irq::gsp::quiesce`` and ``GspIrq::new``, so the whole boot sequence runs with no
interrupt registered. That covers every ``send_command``,
``send_command_no_wait``, and ``await_msg`` call in ``gsp/boot.rs``,
``gsp/commands.rs``, and ``gsp/sequencer.rs``. Probe's own ``Cmdq::drain`` after
``GspIrq::new`` can race the IRQ thread, and ``gspq`` covers that race: both are
consumers, both take the lock, and neither has a waiter to satisfy.

A message becomes a reply as follows:

::

    lock gspq
        read the GspMsgElement header
        validate the framing and the checksum
        lock inflight
            find the entry whose rpc_seq matches
            if found:   copy the element into that entry's reply buffer,
                        then complete it
            if not:     remember that this is an event, or a stale reply
        unlock inflight
        advance the CPU read pointer
    unlock gspq
    dispatch the event, if it was one

The consumer copies the reply out of the ring rather than decoding it in place,
and it copies before it releases the ring slot.
``MessageFromGsp::read`` is generic over the message type, and the consumer has
only the runtime function code, so it cannot call ``read``. It copies the raw
bytes, and the woken sender runs ``M::read`` on them, outside every lock. That
also moves the decode cost off the consumer, which serves every waiter.

The memory comes from the sender, allocated before it takes any lock. The
consumer allocates nothing, which keeps allocation failure and reclaim sleeps out
of the ``gspq`` and ``inflight`` critical sections that every waiter needs.
``GSP_MSG_QUEUE_ELEMENT_SIZE_MAX`` is the bound: nova-core's receive path reads
exactly one queue element per message and never reassembles continuation records,
so 64 KiB is the largest reply it can receive at all. A sender that has no better estimate allocates
that much with ``KVVec``, which falls back to vmalloc rather than demanding an
order-4 page allocation. Adding an associated ``MAX_REPLY_LEN`` to
``MessageFromGsp`` lets the common command allocate a few hundred bytes instead,
with 64 KiB as the fallback for a type that does not declare one.

The consumer must reject a header that claims more than
``GSP_MSG_QUEUE_ELEMENT_SIZE_MAX``. Today's ``wait_for_msg`` checks the advertised
length only against the size of the readable area, so the length a header claims
is unbounded except when it exceeds what is readable, which poisons the queue. With a
fixed-size reply buffer the check has to be explicit, and the failure is the same
framing failure that any other bad header produces.

The in-flight command table
---------------------------

``InFlightCommands`` is a fixed array of ``MAX_IN_FLIGHT`` entries, each holding
an RPC sequence number, a deadline, a reply buffer, and a ``Completion``. Lookup
scans the array linearly under ``inflight``, and neither sleeps nor allocates.

The size is a driver constant rather than something derived from the ring. Ring
capacity does not bound outstanding RPCs: a command's slot is freed as soon as the
GSP reads it, long before the reply arrives, so the table is the only bound on how
many commands can be awaiting a reply. ``MAX_IN_FLIGHT`` is set to 64, above the 62
usable pages of the 63-page ring, so a burst that fills the ring does not also
exhaust the table. A sender that finds the table full returns ``EBUSY`` as a
backpressure signal, and ``MAX_IN_FLIGHT`` can be raised if a future workload
needs more entries.

Timeout and reclaim
-------------------

A timed-out entry is reclaimed by the sender that owns it, under ``inflight``,
immediately on timeout. The sender then returns ``ETIMEDOUT``. A reply that
arrives afterwards finds no matching entry, and the consumer logs it at warning
level and drops it, which is what ``receive_msg`` does today.

Removal and completion are both under ``inflight``, which settles the race
between a sender giving up and the consumer filling its entry. Either the
consumer fills and completes first, in which case the sender checks its
completion once more after the timeout and takes the reply, or the sender removes
first, in which case the consumer finds nothing.

``rpc_seq`` is a ``u32``, so it wraps. A reply can reach the wrong waiter only if
``rpc_seq`` advances all the way around while an entry stays live. An entry is
live for at most the command deadline, and ``CmdqInner::send_command`` advances
``rpc_seq`` exactly once per logical command, so misrouting needs 2^32 commands
within 6 seconds, or 716 million commands per second. Encoding a generation and a
table index into ``rpc_seq`` would remove even that bound, but it would collide
with the continuation-record rule described under "Multi-element commands", so the
design keeps a plain counter and states the bound.

Poisoning
---------

``GpuState::Dead`` replaces ``CmdqInner::poisoned``.

A message carries its length inside the region the checksum covers, so once the
framing or the checksum fails there is no trustworthy length with which to skip
the message. The queue cannot be advanced past it, which is exactly the condition
``Dead`` describes: nothing short of a device reset recovers. Using one state for
both an unadvanceable queue and a crashed GSP removes a field, and it fails every
waiter at once instead of failing each later receive on its own.

The drain-failure path in ``handle_threaded`` joins it. Today that path disables
the GSP vector and logs, which stops the consumer and leaves every sender to time
out one by one. With the state machine it also transitions to ``Dead``, so the
senders fail immediately and with the right error.

The interrupt path
==================

The top half
------------

``GspInterrupt::handle`` runs in hard-IRQ context. It reads and clears the GSP
leaf bit, reads the falcon SWGEN0 status and masks any cause it cannot service,
retriggers the falcon, and rearms PCI interrupt delivery. All of those are
register accesses, all of them unconditional, and none of them may sleep.

It takes no lock and enters no state. Interrupt delivery is disabled while the
GPU is unreachable, so the handler needs no state check of its own:

* Before unbind, the field order in ``NovaCore`` puts ``_gsp_irq`` first, so it is
  dropped first: ``free_irq`` waits out any in-flight handler before the GSP is
  unloaded and before the BAR mapping is released.
* Within ``GspIrq``, ``_leaf_guard`` is declared before ``reg``, so dropping the
  guard disables the GSP vector before ``free_irq`` runs.
* Before a power transition, the ``Suspending`` transition disables the GSP vector
  at its leaf and clears the serviced subtrees at ``TOP``, so GIN raises nothing,
  and then calls ``ThreadedRegistration::synchronize()``, which wraps
  ``synchronize_irq()`` and waits out any handler already running. Only then is
  the GPU powered down.

nova-core allocates MSI or MSI-X and never INTx, so the line is not shared and no
other device's interrupt reaches this handler.

The alternative, a runtime check in the top half, would need a non-sleeping
acquire form, and it would still be wrong if the device lost power between the
check and the register write. Disabling the source avoids both problems.

The IRQ thread
--------------

``GspInterrupt::handle_threaded`` calls ``try_enter``, and on failure returns
without draining. On success it drains, taking ``gspq`` for one message at a time
and ``inflight`` for one entry at a time.

It never takes ``cpuq``, which is what stops it waiting behind a sender: its
longest wait becomes one message copy rather than one RPC round trip. A drain of a full
62-page ring copies at most 62 elements, and it releases ``gspq`` between
elements, so a sender is never shut out for the length of a whole drain.

The state inventory
===================

Everything in nova-core that is written once during construction and read-only
afterwards needs no lock, and is not listed individually: the chipset ``Spec``,
the firmware images, the HAL pointers, the register definitions, the DMA
addresses, and the libos and rmargs allocations are all in that class. The table
covers the state that is mutable, or that a second context can reach.

Order numbers refer to "The lock order" below. The first table names the lock by
its field name only, and "Lock names" carries the types.

==============================  ================  ===============  ==================================  =====
State                           Owner             Lock             Contexts                            Order
==============================  ================  ===============  ==================================  =====
``GpuAccess::word``             ``NovaCore``      ``Atomic<u32>``  probe, unbind, process, IRQ thread  1
transition ownership            ``GpuAccess``     ``state_lock``   transitions, blocking callers       1
``Fsp`` EMEM and mailboxes      ``GspResources``  ``fsp``          probe, unbind, a future resume      6
CPU-to-GSP ring write pointer   ``Cmdq``          ``cpuq``         senders                             7
``elem_seq``, ``rpc_seq``       ``Cmdq``          ``cpuq``         senders                             7
CPU read pointer into ``gspq``  ``Cmdq``          ``gspq``         the consumer                        8
in-flight command table         ``Cmdq``          ``inflight``     senders, the consumer               9
==============================  ================  ===============  ==================================  =====

``GpuAccess::word`` needs no separate lock because one compare-and-exchange
checks the state and increments the holder count together.
``state_lock`` is a slow path. Only a transition owner and a blocking caller
waiting out a transition take it, and the ``CondVar`` named ``changed`` waits on
it. "The receive path" says which thread is the
consumer in each state.

``CmdqInner::poisoned`` is in neither table because ``GpuState::Dead`` replaces
it, and the state word already covers that.

The state that needs no lock at all:

============================  ================  ===============================  =============================
State                         Owner             Why no lock is needed            Contexts
============================  ================  ===============================  =============================
``gsp_mem``                   ``Cmdq``          immutable once allocated         both ring paths
``dev``                       ``Cmdq``          an ``ARef`` written once         any, including the IRQ thread
GIN ``LEAF(i)`` pending bits  hardware          write-1-to-clear per bit         top half, probe drain
GIN ``LEAF_EN``, ``TOP_EN``   hardware          per-bit set and clear            top half, IRQ thread, probe
``logs`` DMA buffers          ``Gsp``           the driver never writes them     GSP DMA, userspace reads
``unload_bundle``             ``GspResources``  ``&mut`` in ``PinnedDrop``       unbind
``vgpu``, the vGPU state      ``GspResources``  written during construction      probe, unbind
``gsp_static_info``           ``Gpu``           written during construction      probe
``sysmem_flush``              ``Gpu``           written once at registration     probe, unbind
``DEBUGFS_ROOT``              the module        written in module init and exit  module init and exit, probe
``AUXILIARY_ID_COUNTER``      the module        ``Atomic<u32>::fetch_add``       probe
============================  ================  ===============================  =============================

Four rows need more than a cell.

The GIN tree registers need no lock. Every enable and disable is a separate set
or clear register in which writing a 1 acts on that bit and writing a 0 leaves it
alone, so no caller ever performs a read-modify-write. The one read-then-write
pair is servicing a leaf: ``Tree::read_pending`` returns a ``LeafPending``, and
``LeafPending::clear_vectors`` writes back only the named bits. Two handlers
servicing different vectors in the same leaf clear disjoint bits, so neither
loses the other's. Three operations are probe-time only:
``LeafPending::clear``, which writes back everything that was read, and
``Tree::disable_all_leaves`` and ``Tree::drain``, which reach subtrees nova-core
does not service. ``Tree::drain``'s doc comment already says so.

Servicing a second subtree does not change that. Under MSI-X each subtree has its
own table entry and its own handler, and ``PciIrqRearmMethod::TopEnableCycleSubtree``
restricts the rearm to the handler's own ``TOP`` bit, so two handlers cycle
disjoint bits. Under MSI there is one message and one handler for the whole tree.
A second subtree does require that the whole-tree operations stay probe-time,
because they cross subtree boundaries.

``Fsp`` is the one place where the existing type signature blocks a resume path.
``Fsp::send_sync_fsp`` takes ``&mut self``, which works today only because
``Fsp`` is touched during ``GspResources`` construction and again in
``PinnedDrop``, both of which have unique access. A resume path reaches it from a
shared reference, so it needs a lock of its own. The FSP and the GSP get separate
locks because they are separate microcontrollers with separate mailbox protocols.
The lock order places ``fsp`` before the GSP queue locks because the boot sequence
holds ``&mut Fsp`` across GSP RPCs: ``GspBootContext`` carries
``fsp: Option<&'ctx mut Fsp<'gpu>>`` while ``hal.boot`` sends commands. That is
also why the longest ``fsp`` hold is a whole boot or resume rather than one
mailbox exchange.

``Gsp::logs`` is deliberately unlocked. The three buffers are byte queues that
GSP-RM writes by DMA and that userspace reads through
``ScopedDir::read_binary_file``. The driver never writes them and never
interprets them, and a decoder already has to cope with a partially written
record, because the put pointer at offset 0 is written by the GSP without any
handshake. Adding a lock would not make a read atomic against DMA. A read is safe
while the GPU is suspended, too: the buffer stays mapped and the GSP is not
writing.

The lock order
==============

The locks have one total order, outermost first. A path may skip levels but never
take them in the other order.

::

    1  gpu_access        the GPU access state
       |
       |    (2..5 do not exist yet)
       v
    2  clients           per-client state                        future
       |
       v
    3  address_spaces    per-VM page tables                      future
       |
       v
    4  channels          channel-ID allocation, per-channel      future
       |                 push buffer state
       v
    5  vram              the VRAM allocator                      future
       |
       v
    6  fsp               the FSP falcon mailbox
       |
       v
    7  cpuq              the CPU-to-GSP ring
       |
       v
    8  gspq              the GSP-to-CPU ring
       |
       v
    9  inflight          the in-flight command table

Every subsystem lock comes before the queue locks, because a subsystem may send a
GSP command while holding its own lock. That leaves the queue locks innermost,
which is what lets the IRQ thread take them without taking a subsystem lock.

The paths through it:

===================================  ==========================================
Path                                 Levels taken
===================================  ==========================================
a sender                             1, then 7, then 9 nested inside 7
a sender waiting for its reply        1 only, with 7 and 9 released
the consumer                         1, then 8, then 9 nested inside 8
the top half                         none
a transition                         1 only, plus whatever it drives
the FSP boot and resume path         1, then 6, then 7 and 9 inside 6
===================================  ==========================================

``inflight`` is nested inside both ``cpuq`` and ``gspq``, and neither of those is
ever taken inside the other. A sender takes ``cpuq`` and never ``gspq``. The
consumer takes ``gspq`` and never ``cpuq``. Nothing in nova-core sends a command
from inside a drain, and nothing may start to: a path that reacts to a GSP event
by sending a command must return from the drain first and send from process
context.

Deadlock and priority inversion
===============================

The IRQ thread behind a sender
------------------------------

This is the inversion the design exists to remove.

Today ``GspInterrupt::handle_threaded`` calls ``Cmdq::drain``, which takes the
same ``inner`` that ``Cmdq::send_command`` holds across a whole send-and-receive
cycle. The IRQ thread waits up to ``RECEIVE_TIMEOUT`` behind a sender.
On a ``PREEMPT_RT`` kernel, or on any kernel booted with ``threadirqs``, the IRQ
thread is a real-time task, so a lower-priority sender that owns the lock inverts
the priority of every GSP notification behind it. A mutex does supply priority
inheritance, which bounds the wait by the sender's own progress, and the sender's
own progress is a 5-second poll for a reply.

After the split, the IRQ thread takes ``gspq`` and ``inflight``, and a sender
takes ``cpuq`` and ``inflight``. The IRQ thread's longest wait is one message
copy or one table entry, both bounded and neither sleeping. A sender no longer
holds anything the IRQ thread needs while it waits for a reply.

nouveau's two locks do not solve this. Its bottom half,
``r535_gsp_msgq_work``, takes the same ``gsp->cmdq.mutex`` that
``r535_gsp_rpc_push`` holds from the head element through the reply, so it blocks
behind a sender the way today's nova-core IRQ thread does. "nouveau" under
"Comparison with the reference drivers" has the call sites.

ABBA candidates
---------------

``cpuq`` against ``gspq``
    The only cycle the split makes structurally possible. Taking either lock
    while holding the other would create it. Neither path does: the two rings have
    disjoint pointer sets, so neither needs the other's lock. The rule that keeps
    it that way: no code sends a command from inside a drain.

``inflight`` against ``cpuq`` or ``gspq``
    ``inflight`` is the innermost level and nothing is taken inside it, so it
    cannot be the first half of a cycle.

``gpu_access`` against any mutex
    A holder takes mutexes, and a transition owner waits for holders. If a
    transition owner held one of those mutexes while waiting, a holder that
    needed it could never exit and the wait would never finish. So a transition
    owner takes no queue lock across its wait for the holder count. It acquires
    them only after the count reaches zero, when it is the only thread inside.

``fsp`` against ``cpuq``
    The boot path holds ``&mut Fsp`` across GSP RPCs, so ``fsp`` is outside
    ``cpuq``. Nothing sends an FSP message from inside a GSP RPC, and nothing
    may start to.

``state_lock`` against ``changed``
    ``changed`` is a ``CondVar``, so a waiter releases ``state_lock`` before
    sleeping and reacquires it on wake. A transition publishes the new state under
    ``state_lock``, releases it, and then notifies, so a waiter that read the old
    state cannot miss the notification: it re-checks the state word under
    ``state_lock``, and a transition cannot publish between that check and the
    sleep.

Sleeping while holding
----------------------

===================================  ============  ==================================
Held                                 May sleep     Longest sleep
===================================  ============  ==================================
``gpu_access``                        yes           ``RECEIVE_TIMEOUT``, 5 seconds
``cpuq``                              no            none, the space wait is outside
``gspq``                              no            none
``inflight``                          no            none
``fsp``                               yes           the FSP mailbox poll timeout
===================================  ============  ==================================

The first row is the requirement rather than an oversight, and it is why an
RCU-backed primitive such as ``Revocable`` cannot implement the GPU access
state.

The contract for nova-drm
=========================

nova-core exposes no locks to a second-level driver. It exposes a set of entry
points and a statement about what they may do.

Every nova-core entry point that reaches the GPU may sleep
    Each one enters the GPU access state, which can wait out a resume, and an RPC
    then waits up to ``RECEIVE_TIMEOUT`` for a reply. No nova-core entry point may
    be called from atomic context: not from a spinlock-held region, not from an
    RCU read-side critical section, and not from a hard-IRQ handler. A caller may
    call from a threaded IRQ handler, from a work item, and from an ioctl.

A caller may hold its own locks across a call
    nova-core takes no lock that a caller can also take, and it makes no callback
    into a caller while holding one of its own. A caller's lock never closes a
    cycle with a nova-core lock. A caller must not hold a lock that cannot
    stay held across a multi-second sleep.

Two cases need spelling out
    A caller may hold a ``drm_dev_enter()`` SRCU section across a nova-core call,
    because the SRCU read side may sleep, but it should not: ``drm_dev_unplug()``
    then waits for the RPC, which can be 5 seconds. A caller must not call
    nova-core from inside a ``dma_fence`` signalling critical section, where
    neither a ``GFP_KERNEL`` allocation nor a multi-second wait is permitted. A
    future async Vulkan queue has to keep its RPC-issuing work off the signalling
    path.

An error means what it says
    ``EAGAIN`` means the GPU is mid-transition, so the caller may retry.
    ``ENODEV`` means the GPU is off or dead, so it may not. ``ETIMEDOUT`` means the
    GSP did not reply within ``RECEIVE_TIMEOUT`` and the command's effect is
    unknown. ``EBUSY`` from a send means the in-flight table is full, which is
    backpressure.

Extending the model
===================

Each future area names the lock it adds and where it goes. The order in "The lock
order" already has a level reserved for each.

Channels and doorbells
    ``channels: Mutex<ChannelIds>`` for the channel-ID allocator, and a
    ``Mutex<ChannelRing>`` per channel for the push buffer's put pointer. Level 4,
    above the queue locks, because creating and destroying a channel is a GSP RPC.
    A doorbell write is a single register write to a per-channel doorbell address
    and needs no lock beyond the channel's own, but it does need the GPU access
    state, because a doorbell rung at a powered-down GPU is a write to a dead
    BAR.

MMU and page-table management
    ``address_spaces: Mutex<AddressSpaces>`` for the set of address spaces, and a
    per-address-space ``Mutex<PageTables>`` for one space's tables. Level 3.
    ``Documentation/gpu/nova/core/todo.rst`` records that nova-drm needs
    fine-grained control here to implement asynchronous Vulkan queues, and the lock
    order constrains how: a page-table update that requires a GSP RPC takes
    level 3 and then the queue locks, so any such update can block for
    ``RECEIVE_TIMEOUT``. An update on a fence-signalling path must not need an
    RPC. Splitting page-table work into an RPC-requiring part and a purely local
    part is the design work the lock order forces.

VRAM allocation
    ``vram: Mutex<VramAllocator>``. Level 5, below the page tables and above the
    queue locks, because an allocation may need a GSP RPC and a page-table update
    consumes an allocation.

Asynchronous Vulkan queues with multiple clients
    ``clients: Mutex<Clients>`` for the client table, and per-client state under
    the client's own lock. Level 2, the outermost of the future levels, because a
    client owns address spaces, which own page tables, which consume VRAM. Multiple
    clients work because the queue locks are innermost and short: two clients'
    RPCs overlap in the GSP rather than serializing in the driver, which is the
    property the in-flight table provides.

vGPU
    ``vfs: Mutex<VfSlots>`` for the virtual-function slots, at level 2 alongside
    ``clients``. There is one physical GPU and one ``GpuAccess``: a VF
    tree is a separate GIN tree, not a separate power domain.

Runtime PM with autosuspend
    A runtime-PM usage count would count the same threads the holder count
    already counts, so ``GpuAccess::enter`` takes the runtime-PM reference
    alongside the state and exiting drops it with ``pm_runtime_put_autosuspend``.
    The two counts stay in step because one pair of functions owns both, which is
    the property a separate ``pm_runtime_get`` at each call site would not have.
    ``pm_runtime_get_sync`` becomes ``enter``, and once a resume path exists,
    ``enter`` starts a resume in ``Off`` instead of failing.
    ``pm_runtime_get_if_active`` becomes ``try_enter``. The autosuspend idle
    callback takes a ``Suspending`` transition, which already waits for the holder
    count to reach zero. No new state is needed, because ``Suspending``, ``Off``,
    and ``Resuming`` are the states runtime PM cycles through and they are
    modelled from the start.

Comparison with the reference drivers
=====================================

The open-source GSP-RM driver
-----------------------------

It serializes with two locks. The API lock is one global sleeping reader-writer
lock that serializes RM API calls from clients. The GPU lock is a per-GPU binary
semaphore that synchronizes across all IRQ levels. The order is API lock, then
GPU lock, and among GPU locks by ascending GPU instance.

Every RPC send and every RPC poll asserts the GPU lock, and the send-then-wait is
one continuous hold with a busy loop rather than a sleep. There is no lock
specific to the message queue: the GPU lock is what keeps two senders apart, and
it is also what protects the single staging buffer that every caller marshals its
RPC into.

Only one synchronous RPC is outstanding at a time, and that is enforced by
assertion rather than by structure. There is no sequence-number-to-waiter map: the
poll takes the expected function code and sequence number as scalars and compares
them inline. Its own comment explains that a nested RPC would deadlock on the
wrong reply and that the right fix would be to defer the second RPC to a work
item.

The interrupt path takes the same GPU lock. Its comment states that the bottom
half and the RPC poll race, and that the race is avoided only because the bottom
half runs under the GPU lock. It avoids blocking behind an in-flight RPC in three
ways: the top half acquires the GPU lock conditionally and does nothing if it
cannot, a deferred re-run is attempted at lock release, and the polling thread
drains asynchronous events itself on every iteration of its wait.

An RPC timeout is 1.5 times the RM default, which comes to 6 seconds in graphics
mode and 45 in compute mode. There is nothing to reclaim on timeout because there is no
table. The late reply arrives as an unexpected event and is logged and dropped.
Three consecutive timeouts mark the device for reset.

The purpose of nova-core's design is to have neither of those two locks. There is
no global lock, because there is no state shared between GPUs to protect. And
there is no per-GPU lock, because the state a per-GPU lock would cover is split
into the pieces this document names, each with its own short critical section.
The interrupt path and the RPC path stay apart because they take different
locks, which is the GPU lock's role in the reference driver.

nouveau
-------

nouveau declares both ``gsp->cmdq.mutex`` and ``gsp->msgq.mutex``, but their
names do not correspond to a send lock and a receive lock. ``cmdq.mutex`` covers
both directions and ``msgq.mutex`` covers only the notifier registry.
``r535_gsp_rpc_push`` takes ``cmdq.mutex`` before the head element of a
multi-element command and releases it after the reply.
``r535_gsp_rpc_poll`` takes it around ``r535_gsp_msg_recv``, and
``r535_gsp_msgq_work``, the bottom half, takes it around its own drain. nouveau
has the same send-and-wait serialization nova-core has, and its second lock does
not address it.

nova-core's receive path should copy two nouveau behaviors. nouveau copies a
message out of the ring into a ``kvmalloc``'d buffer before advancing the read
pointer rather than decoding in place, which is what a single consumer has to do,
and it peeks the header to learn the length before allocating, which is safe
because only one thread advances the read pointer.

nova-core should not copy nouveau's reply matching, which compares the function
code and sequence number inline in the receiving thread and routes everything else
through a notifier registry. That works while one thread at a time waits for a
reply and does not extend to several.

Verification
============

Lockdep
-------

Each mutex is created with ``new_mutex!``, which supplies a distinct static lock
class per declaration site. With ``CONFIG_PROVE_LOCKING`` set, lockdep records
the order the first time each pair is taken and reports an inversion the first
time one occurs, whether or not it deadlocks. The pairs it records are
``inflight`` inside ``cpuq`` and ``inflight`` inside ``gspq``. The inversions it
would catch are ``cpuq`` taken inside ``gspq`` or the reverse, either queue lock
taken inside ``inflight``, and any future level 2 through 6 lock taken inside a
queue lock.

``GpuAccess`` is not a lockdep-tracked lock, so lockdep does not see an inversion
between it and a mutex. Two things narrow that gap. ``enter`` calls
``might_sleep()``, so ``CONFIG_DEBUG_ATOMIC_SLEEP`` catches a caller that enters
from atomic context, which is the mistake that matters most. And one function carries the transition
rule, which is that a transition owner holds no queue lock across its wait for
the holder count. Checking it means reading that function rather than every call
site.

A lockdep map for a non-lock construct would close the rest of the gap. It needs
``lockdep_init_map``, ``lock_acquire``, and ``lock_release`` in ``rust/kernel``,
which do not exist there today. That is not proposed here, because it is not
needed to make the design correct, and the fallback if it were rejected is a
debug-only counter of held queue locks per task with a ``WARN`` in ``enter``.

Runtime abstractions
--------------------

The design uses only what ``rust/kernel`` provides today. No new abstraction is
required for the surface this document covers. Three are plausible and none is
necessary:

* ``rw_semaphore`` is not needed, because the state word plus a condvar does the
  job and carries the state as well.
* An SRCU wrapper is not needed. The one SRCU use in Rust today is
  ``drm_dev_enter()`` and ``drm_dev_exit()``, wrapped as
  ``drm::device::RegistrationGuard``, and it is not the pattern the GPU access
  state needs: it is a one-way transition to unplugged, with no way back, and it
  says only whether the device is registered rather than which of six states it is
  in. A cycle through suspend and resume needs a state, and a state needs an
  ordinary variable beside the SRCU section, which is the combination the state
  word replaces.
* ``synchronize_irq()`` is already wrapped, as
  ``irq::ThreadedRegistration::synchronize()``, so the suspend path needs nothing
  new.

Two additions will be wanted when the PM callbacks land, and neither is needed
before then.

The first is an uninterruptible ``CondVar::wait_timeout``. A PM callback must not
be interruptible, and ``CondVar`` today offers only an interruptible timed wait
and an untimed uninterruptible one. The private ``wait_internal`` already takes
both the task state and the timeout, so the addition is a public wrapper. The
fallback is a fresh ``Completion`` per transition, allocated by the transition
owner, which is necessary because ``Completion`` has no reinitialize and
``complete_all`` is permanent.

The second is a runtime-PM abstraction, without which ``enter`` cannot take a PM
reference. The fallback is to keep the holder count and the PM usage count
separate and require every ``enter`` call site to pair with a ``pm_runtime``
call, which is the arrangement that lets the two disagree.

KUnit coverage
--------------

``GpuAccess`` and ``InFlightCommands`` are pure software and are testable without
a GPU. The design keeps them that way on purpose: neither one takes a ``Bar0`` or
a ``device::Device``, so a test can construct either.

``GpuAccess``
    Every legal transition and every illegal one. The return value of ``enter``
    and ``try_enter`` in each of the six states. That a transition does not
    complete while a holder exists, and does complete when the last one exits.
    That a ``Dead`` transition fails a waiting holder rather than waiting for it.
    That two transitions cannot overlap.

``InFlightCommands``
    Insert, match by sequence number, reclaim on timeout, and the full-table
    ``EBUSY``. That a reply arriving after its entry was reclaimed matches
    nothing. That a reply arriving for a sequence number the table never held
    matches nothing. The sequence-number arithmetic across a ``u32`` wrap.

KUnit cannot cover the ring pointers, which need a GSP to move them, or the
interrupt path, which needs hardware. Lock order is lockdep's job and is
exercised by any ``CONFIG_PROVE_LOCKING`` run that sends a command and takes an
interrupt, which the existing interrupt self-test already produces.

What is not confirmed
---------------------

Two questions depend on GSP firmware behavior that the available source does not
settle.

Whether the GSP validates the element sequence number
    The ``seqNum`` field of a queue element advances once per element in
    nova-core, in nouveau, and in the firmware ABI nova-core targets. Whether the
    GSP requires successive elements to increase by exactly one, or uses the field
    only for diagnostics, is not visible: the current open-source GSP-RM tree has
    moved to a later message-queue protocol that has no per-element sequence
    number at all, and no firmware-side check for one appears in it. The test that
    would settle it: publish two commands with their element sequence numbers
    deliberately swapped, so element ``N+1`` reaches the ring before element
    ``N``, and observe whether the GSP replies to both, to neither, or logs a
    framing error in ``logrm``. Until that runs, the design keeps publish order
    equal to ``elem_seq`` order, which the serialized slot reservation supplies
    with no extra synchronization.

Whether continuation records must carry consecutive RPC sequence numbers
    The GSP-side source of a later ABI does require it: it compares each
    continuation record's RPC sequence number against the previous one plus one,
    and on a mismatch stashes the interloping head and fails the multi-element
    command with an invalid-request error. Both reference drivers assign
    accordingly. nouveau increments its RPC sequence counter for the head and for
    every continuation record, and the open-source GSP-RM driver asserts that the
    last sequence number equals the first plus the record count. nova-core assigns
    the same ``rpc_seq`` to the head and to every continuation record, which
    diverges from both. Whether the ABI nova-core targets enforces the same rule
    is not confirmable from the source available. The test that would settle it:
    send a command whose payload exceeds one queue element and check whether the
    GSP replies or logs a continuation-sequence error. This is a defect in the
    continuation path rather than in the locking, and it is not addressed by this
    design.

Lock names
==========

==============  ==============================  ==================================  =====
Name            Type                            Protects                            Order
==============  ==============================  ==================================  =====
``gpu_access``  ``Arc<GpuAccess>``              the ``GpuState`` and holder count   1
``state_lock``  ``Mutex<()>`` in ``GpuAccess``  transition ownership                1
``fsp``         ``Mutex<Fsp<'gpu>>``            the FSP EMEM window and mailboxes   6
``cpuq``        ``Mutex<CpuQueue>``             the CPU-to-GSP ring and sequences   7
``gspq``        ``Mutex<GspQueue>``             the CPU read pointer into ``gspq``  8
``inflight``    ``Mutex<InFlightCommands>``     commands awaiting a reply           9
==============  ==============================  ==================================  =====

Levels 2 through 5 are reserved for ``clients``, ``address_spaces``,
``channels``, and ``vram``. They have no code behind them yet, and they are named
here so that the first patch to add one does not have to settle the order
again.

A ``Mutex<T>`` is named for the data it wraps, so ``cpuq.lock()`` reads as "lock
the CPU-to-GSP queue" and there is no ``lock`` in the field name. ``state_lock``
is the one ``Mutex<()>``, and it takes the ``_lock`` suffix because it protects no
typed data. ``cpuq`` and ``gspq`` are the names ``GspMem``
already gives the two rings. ``changed`` is a ``CondVar`` rather than a lock and
so does not appear above.

References
==========

* nova-core source: the command queue in ``gsp/cmdq.rs``, the GSP event handler
  in ``irq/gsp.rs``, the tree API in ``irq/interrupt_tree.rs``, and the FSP
  interface in ``fsp.rs``.
* ``Documentation/gpu/nova/core/interrupts.rst`` for the interrupt vocabulary and
  the delivery rules the top half follows.
* ``Documentation/gpu/nova/core/todo.rst`` for the MMU and page-table entry that
  fixes the fine-grained-locking requirement nova-drm has.
* ``rust/kernel/sync/`` for the primitives, ``rust/kernel/revocable.rs`` and
  ``rust/kernel/devres.rs`` for the RCU-backed alternative, and
  ``rust/kernel/drm/device.rs`` for the one SRCU pattern already wrapped in Rust.
