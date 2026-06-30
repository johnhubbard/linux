.. SPDX-License-Identifier: GPL-2.0
.. SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

=================================================
GPU interrupt handling: GIN and the GSP event
=================================================

This document describes how nova-core receives interrupts from the GPU on Turing
and later parts. It covers the GPU Interrupt and Notification unit (GIN), which
is the GPU's interrupt controller, and the GSP event interrupt.

Throughout, *CPU* means the CPU and the nova-core driver running on it. The GPU
also has on-chip processors that run their own firmware and receive their own
interrupts, and the GSP (GPU System Processor) is one of them.

The register names in this document are the names from the GPU hardware
reference headers. The CPU tree's registers live in the per-function
``NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_*`` aperture on every supported part, and
the controller itself has a second name on pre-Hopper parts (see "Register
naming").

Terminology
===========

Three different numbers are all called a "vector" in the surrounding material.
This document gives each one its own name and never uses "vector" on its own.

GIN vector
    The GPU-internal interrupt source number, 0 through 511 on Hopper. It is a
    bit address within the tree: leaf ``vector / 32``, bit ``vector % 32``. The
    CPU doorbell is GIN vector 129 and the GSP event is GIN vector 155.

MSI-X entry
    An index into the device's MSI-X table, 0 through 7 on Hopper. Linux's
    ``struct msix_entry`` names its Linux IRQ number ``.vector``, which is a
    third meaning.

Linux IRQ number
    What ``request_irq()`` takes, obtained from ``pci_irq_vector()``.

The remaining terms, each named for the register or the specification that owns
it:

enable / disable a GIN vector
    ``LEAF_EN_SET`` and ``LEAF_EN_CLEAR``.

enable / disable a subtree
    ``TOP_EN_SET`` and ``TOP_EN_CLEAR``.

serviced subtree
    A subtree nova-core enables and has a handler for.

rearm
    Restoring PCI interrupt delivery after servicing an interrupt. It is a
    ``TOP_EN`` disable-then-enable cycle everywhere except under pre-Hopper
    MSI, where it is a write to the end-of-interrupt (EOI) register in the BAR0
    configuration-space mirror (see "Rearming PCI interrupt delivery").

mask
    Reserved for the two places hardware and the PCI specification use the
    word: the MSI-X per-entry Vector Control mask bit, which Linux owns, and
    the falcon cause masks. It never names a GIN enable.

latched, pending
    A ``LEAF`` bit records its source whether or not the GIN vector is enabled.
    A disabled vector's pending bit never appears in ``TOP``.

clear a leaf vector
    Write a 1 to the vector's bit in ``LEAF``. Open RM spells the same
    operation ``intrClearLeafVector_HAL``.

pending bits
    The plain bitmask value read from a ``LEAF`` register.

unit
    A generic interrupt-raising block. "Engine" is reserved for the blocks that
    do usermode work: GR, CE, NVDEC, and the like.

The GIN controller
==================

A GPU has many interrupt sources: the GSP, copy engines, the graphics engine,
video decode and encode, the MMU fault path, timers, and others. Each one has a
GIN vector number, which is internal to the controller and is not a PCI vector
index.

GIN records which vectors are pending in its own two-level register tree and
raises the PCI interrupt when an enabled vector becomes pending. The CPU's
handler reads that tree to tell the sources apart, clears the pending vectors,
and runs the work for each.

How the tree reaches the CPU over PCI
-------------------------------------

How many PCI interrupts the tree needs depends on the interrupt type Linux
grants.

MSI has a single message, and every subtree raises that one message. One
allocated vector serves the whole tree.

MSI-X raises a separate table entry per subtree, so a subtree's interrupts
arrive on the table entry whose index is the subtree number. Linux masks each
table entry a driver did not allocate, and a masked entry sends no message: the
request sets a bit in the pending-bit array and waits for an unmask that never
comes. A driver that leaves out the entry its subtree raises loses every
interrupt on that subtree, and loses it silently, with the GIN leaf and TOP
registers showing the vector pending and enabled while no handler runs.

The serviced-subtree invariant
------------------------------

Every subtree enabled at TOP must have an allocated PCI vector with a registered
handler.

MSI satisfies this with one message that every subtree raises. MSI-X needs one
allocated, unmasked entry per serviced subtree, and a PCI allocation cannot be
sparse, so it runs from entry 0 through the highest serviced subtree::

    MSI-X, with subtree 2 serviced:

      subtree 0  ->  entry 0   allocated, no handler, stays masked
      subtree 1  ->  entry 1   allocated, no handler, stays masked
      subtree 2  ->  entry 2   handler here, and its rearm covers subtree 2

    MSI, with any serviced set:

      every serviced subtree  ->  the one allocated vector, whose handler's
                                  rearm covers the whole serviced set

The entries allocated below a serviced subtree that the driver does not service
cost nothing: Linux unmasks an entry only when its interrupt is requested, and a
disabled subtree raises nothing.

nova-core services exactly one subtree, subtree 2, because both the vectors it
uses are in leaf 4: the GSP event (155) and the self-test doorbell (129). That
is also the subtree the resource manager assigns to its ``UVM_SHARED`` interrupt
category on every chipset nova-core supports.

Interrupt trees
===============

GIN keeps a separate interrupt tree for each place an interrupt can be sent to:

* One tree per PCIe function. The Physical Function (PF) has a tree, and each
  Virtual Function (VF) has a tree.
* One tree per on-chip microcontroller that receives interrupts, starting with
  the GSP.

Each destination reaches its own tree through its own BAR0 and cannot reach any
other tree. GSP firmware selects the tree each unit's interrupt is sent to.

nova-core services the CPU tree of one function. The VF trees and the
microcontroller trees belong to firmware or to virtual functions.

The two-level tree
==================

Each tree has two levels. The bottom level is the LEAF registers, which hold one
pending bit per vector. The top level is the single TOP register, which
summarizes the leaves.

* Each ``LEAF(i)`` is a 32-bit register holding the pending bits for vectors
  ``i * 32`` through ``i * 32 + 31``. A set bit means that vector is pending.
* ``TOP`` is a single 32-bit read-only register. Each of its bits summarizes one
  *subtree*, which is a pair of adjacent leaves. TOP bit ``N`` reflects
  ``LEAF[2N]`` and ``LEAF[2N + 1]`` as filtered by their leaf enables, so a
  vector that latched while disabled does not appear in TOP.

A subtree is two leaves, so a part with L leaves has L / 2 subtrees and uses
that many TOP bits. An 8-leaf part uses TOP bits 0 through 3, and the other 28
bits always read 0. A 16-leaf part uses TOP bits 0 through 7::

    TOP  (one 32-bit register, and an 8-leaf part uses only bits 0..3)

      bit 0  ->  subtree 0  ->  LEAF[0], LEAF[1]   vectors   0..63
      bit 1  ->  subtree 1  ->  LEAF[2], LEAF[3]   vectors  64..127
      bit 2  ->  subtree 2  ->  LEAF[4], LEAF[5]   vectors 128..191
      bit 3  ->  subtree 3  ->  LEAF[6], LEAF[7]   vectors 192..255
      bits 4..31: always 0 on an 8-leaf part (a 16-leaf part uses bits 0..7)

    A LEAF is one 32-bit register, one bit per vector. For example, LEAF[4]
    holds vectors 128..159:

      bit 1  = vector 129  (CPU doorbell)
      bit 27 = vector 155  (GSP event)

Registers
---------

All the registers are 32 bits, defined in ``regs.rs`` under the
``NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_*`` names. The leaf registers are arrays
indexed by leaf number:

* ``LEAF(i)`` holds the pending bits for the vectors in leaf ``i``. Reading
  returns the pending bits, and writing a 1 to a bit clears that vector
  (write-1-to-clear).
* ``LEAF_EN_SET(i)`` and ``LEAF_EN_CLEAR(i)`` enable and disable individual
  vectors in leaf ``i``.
* ``TOP`` is the read-only summary: bit N is set when an enabled vector is
  pending in ``LEAF[2N]`` or ``LEAF[2N + 1]``. A vector that latched while its
  leaf enable was clear does not appear.
* ``TOP_EN_SET`` and ``TOP_EN_CLEAR`` enable and disable subtrees.
* ``LEAF_TRIGGER`` makes a vector pending in software. The self-test uses it.

Mapping a vector to the tree
----------------------------

Each vector occupies one bit of one leaf, and each leaf belongs to one
subtree::

    leaf    = v / 32
    bit     = v % 32
    subtree = leaf / 2

Both of the vectors nova-core names by number fall in leaf 4: vector 129 at bit
1 and vector 155 at bit 27, so both arrive under subtree 2.

Enabling and clearing
---------------------

Each bit of a set or clear register acts on its own: writing a 1 performs the
action for that bit, and writing a 0 leaves the bit's state alone. No caller
ever needs a read-modify-write.

* ``LEAF(i)`` is write-1-to-clear. Reading returns the pending bits. Each bit
  must be cleared before its vector is serviced.
* ``LEAF_EN_SET(i)`` and ``LEAF_EN_CLEAR(i)`` enable and disable individual
  vectors in a leaf.
* ``TOP_EN_SET`` and ``TOP_EN_CLEAR`` enable and disable whole subtrees.

A vector reaches the CPU only when both its leaf enable bit and its subtree's
TOP enable bit are set. The leaf enable governs delivery and the TOP summary,
but not the latch: a disabled vector still latches its LEAF bit, and that bit is
visible only by reading the leaf directly.

How a unit interrupt reaches the CPU
====================================

A unit does not write a LEAF register itself. Each unit has an interrupt routing
register, and GSP firmware programs it once at boot. Firmware writes three
things into it: the unit's VECTOR (which leaf bit it uses), its GFID (which tree
to post to: the PF or a specific VF), and its destination flags (which consumers
get it: the CPU, the GSP, or another on-chip microcontroller).

Later, when a unit has an event, three things happen in turn::

    1. The unit sends an interrupt message to GIN, carrying the VECTOR, GFID,
       and destination flags from its routing register.
    2. GIN sets bit (VECTOR % 32) in LEAF[VECTOR / 32], in the tree that the
       GFID and destination flags select.
    3. If that vector is enabled and its subtree is enabled, GIN raises the PCI
       interrupt to the CPU.

Because firmware assigns the vectors, nova-core does not hardcode which vector
belongs to which unit. The one exception nova-core relies on is the GSP event
vector, which firmware pins to a fixed number (see "The GSP event vector").

Edge behavior and rearm
=======================

The pieces behave as follows:

* A LEAF bit is a latch. It is set on the rising edge of its source and stays set
  until the CPU writes a 1 to it. A source that stays high does not set the bit
  again.
* TOP is read-only and reports the subtree's *enabled* pending state. A vector
  that latched while its leaf enable was clear does not appear in TOP.
* LEAF_EN and TOP_EN are CPU-controlled enables that allow or block delivery.
* GIN raises the PCI interrupt for subtree N when the subtree's enabled pending
  state goes from low to high::

    Per vector, in leaf i at bit b:
        LEAF[i][b] AND LEAF_EN[i][b]

    Per subtree N, across its leaves 2N and 2N + 1:
        OR of every enabled pending bit  ->  TOP[N]

    Delivery for subtree N:
        TOP[N] AND TOP_EN[N]  ->  rising edge  ->  PCI interrupt

    TOP_EN applies below TOP, so disabling a subtree halts delivery and leaves
    what TOP reports unchanged.

Because a disabled vector is invisible in TOP, code that must find every pending
bit cannot descend from TOP. It has to read the leaves directly. Open RM does
the same: its stalling-interrupt path never reads TOP, and instead walks every
subtree it implements reading LEAF registers.

Because delivery is edge-triggered, writing ``TOP_EN_SET`` while an enabled leaf
bit is still set produces a new edge. A full tree walk uses this: after it
clears the leaves, it writes ``TOP_EN_SET`` so an interrupt that arrived during
servicing is still delivered.

A unit that holds an internal level signal high does not produce a new leaf edge
after the CPU clears the bit, so rearming alone does not re-deliver it. Such
units have an ``INTR_RETRIGGER`` register that forces a new edge.

Retriggering a falcon
---------------------

A falcon signals the tree on a transition of its enabled interrupt causes.
Clearing the tree leaf while a cause is still latched leaves no transition, so
the vector stays clear however many further causes arrive. Both clear orders
have that window, so a handler on a falcon vector writes ``INTR_RETRIGGER`` on
every path that services the vector.

That re-emit must not be able to raise a cause that nothing clears. A cause the
handler does not service is removed from the falcon's enabled set with
``IRQMCLR`` and cleared with ``IRQSCLR`` before the re-emit.

``INTR_RETRIGGER`` is absent on Turing falcons and present from GA100 onward, so
the write is conditional on the architecture. A Turing handler cannot supply a
transition that went missing, so it must leave no cause latched: it reads the
status once and takes every cause that status reports, rather than stopping at
the first one it recognizes. A cause left behind holds the falcon's enabled set
non-empty, and no later cause from that falcon signals the tree at all.

One window stays open on Turing. A cause that arrives between the status read
and the clears is not in the status, so it stays latched after the tree leaf has
been cleared. Open RM has the same window: ``kgspService_TU102`` ends with
``kflcnIntrRetrigger``, which is implemented from GA100 onward and does nothing
on Turing.

Rearming PCI interrupt delivery
-------------------------------

Clearing the GIN state is not enough. A message-signaled interrupt is
delivered once per edge, and the PCI side delivers no further interrupt until the
CPU rearms it. Which operation does that depends on the GPU family and on the
interrupt type Linux granted:

==================  =====  ===========================================
Architecture        Type   Rearm operation
==================  =====  ===========================================
Turing through Ada  MSI    write the configuration-mirror EOI register
Hopper and later    MSI    clear then set the serviced TOP enables
Any                 MSI-X  clear then set the handler's own TOP enable
==================  =====  ===========================================

The MSI forms cover every serviced subtree, because one message serves all of
them. The MSI-X form covers one subtree, because each serviced subtree has its
own table entry and its own handler.

INTx is level-triggered and needs no rearm write. nova-core does not allocate it,
so it never reaches a handler.

A handler must rearm once per delivered interrupt, on every path that services
one. A handler that skips the rearm receives no further interrupts at all.

The rearm is separate from the TOP restore at the end of a full tree walk, even
though two of the three forms write the same registers. The walk clears TOP_EN
on entry so that it can read and clear without new interrupts arriving, and sets
it again on exit. For the two enable-cycle forms that restore also rearms, but
pre-Hopper MSI rearms through the configuration mirror, which the walk never
writes, so the startup sequence rearms explicitly after the walk.

Servicing an interrupt
======================

nova-core services the tree in one of two ways, depending on which code handles
the interrupt.

The GSP event handler services one vector, so it leaves its subtree enabled and
reads and clears only its own leaf bit, touching a single leaf per interrupt.

The startup drain walks the whole tree instead, because it must clear whatever is
pending across every subtree rather than one known vector. It disables the
subtrees, clears every pending leaf, then enables them again.

The drain reads every implemented leaf rather than descending from TOP. Boot
latches vectors while they are still disabled, and those bits do not appear in
TOP, so a TOP-driven walk would skip exactly the state the drain has to clear.

The two paths as register operations::

    Full tree walk (the one-time startup drain):
        write TOP_EN_CLEAR = serviced        disable, to stop new interrupts
        for each implemented subtree N, for i in {2N, 2N+1}:
            pending = read LEAF[i]           pending vectors in this leaf
            write LEAF[i] = pending          clear (write-1-to-clear)
        write TOP_EN_SET = serviced          restore TOP_EN

    Notification, subtree stays enabled (the GSP event handler, and the
    self-test, which deliberately mirrors it):
        pending = read LEAF[gsp_leaf]        is our vector's bit set?
        write LEAF[gsp_leaf] = gsp_bit       clear only our bit
        rearm PCI interrupt delivery         see "Rearming PCI interrupt
                                             delivery"

Two rules for the full walk:

* Clear every pending leaf bit, including bits nova-core does not handle. An
  uncleared bit holds its subtree in the pending state, and restoring TOP_EN
  over it produces a delivery edge straight away. The walk writes back every bit
  it read.
* Restore TOP_EN only after clearing every pending leaf. Otherwise a still-set
  bit raises the interrupt again while the walk is still running.

The notification path clears one bit, so a vector pending alongside it in the
same leaf keeps its bit and stays pending for whoever services it. Both paths
must rearm PCI delivery for the interrupt they serviced.

Interrupts and notifications
============================

Two kinds of source use the tree:

* An interrupt means a unit needs servicing.
* A notification means a unit is reporting that something happened, such as a log
  record or completed work.

The GSP event is a notification. Its handler leaves the subtree enabled and
clears only the GSP leaf bit.

The hardware manuals also split the vector space into "stall" and "nonstall"
ranges. Those name address ranges rather than describing behavior. nova-core
does not service the stall range.

Per-architecture differences
============================

The tree is the same on every supported GPU except for its size, and there are
only two sizes, split at Hopper:

===================  ======  ========  ====================
GPUs                 Leaves  Subtrees  Implemented subtrees
===================  ======  ========  ====================
Turing, Ampere, Ada  8       4         ``0x0f``
Hopper and later     16      8         ``0xff``
===================  ======  ========  ====================

Only the lower eight leaves exist before Hopper, so TOP bits 4 through 31 read
zero there. Hopper and later have 16 leaves, though sources do not populate all
of them.

The implemented subtrees bound which TOP bits mean anything. That set is wider
than the set nova-core enables, which is the subtrees it services, per the
serviced-subtree invariant. The startup drain still reads every implemented
leaf, because a vector that latched while disabled is invisible in TOP and can
be in any leaf.

The HAL provides the leaf count, and the subtree count (leaves / 2) and the
implemented-subtree set derive from it. The rearm method is the HAL's other
per-architecture value.

Multi-die parts
===============

On multi-die parts the controller is replicated per die, with an aggregation
level above the per-die TOP registers. nova-core services the CPU tree of one
function on a single-die part, so it does not drive the aggregation level.

The GSP event
=============

When the GSP has output for the CPU (log records, error records, and other
events), it writes the messages into the GSP-to-CPU queue in shared memory and
raises SWGEN0, one of the software-generated interrupt outputs of the GSP
microcontroller (a "falcon" in NVIDIA hardware). SWGEN0 is routed through a GIN
vector, so it reaches the CPU as a PCI interrupt::

    GSP writes messages into the GSP-to-CPU queue
    GSP raises SWGEN0
    GIN sets the GSP leaf bit, and the subtree becomes pending
    PCI interrupt -> Linux IRQ -> nova-core top half, in IRQ context, which
                                 must not sleep:
        read the GSP leaf bit and clear it (subtree stays enabled)
        read the GSP falcon IRQ status, clearing SWGEN0 if it was set
        for every other cause that status reports: report it, then remove it
            from the falcon's enabled set and clear it
        retrigger the falcon
        rearm PCI interrupt delivery
        wake the IRQ thread if SWGEN0 was set
    IRQ thread, which may sleep: take the command-queue lock and drain the
        GSP-to-CPU queue, routing each message

A halt and a posted message can be pending together, so the top half handles
every cause the status reports rather than choosing between them (see
"Retriggering a falcon").

The interrupt is only the trigger to drain the queue. A thread polling for a
command reply routes the messages it reads through the same classifier (see
"Draining and classifying the GSP-to-CPU queue").

If the drain fails, the queue cannot advance past the message it could not parse,
so every later notification would repeat the same failure. The IRQ thread
disables the GSP vector before reporting the failure, which leaves the queue
unserviced until the device is reset.

Enabling the GSP event
----------------------

SWGEN0 is a latch, and the GSP drives no new edge into the tree while it stays
set. GSP boot consumes its notifications by polling the queue, which leaves the
latch set and leaves stale state in the tree, so the handoff from polling to
interrupts has a required order::

    disable every implemented vector    drop enables left by boot or by a
                                        driver that ran before this one
    drain the tree (full walk)          clear stale GIN state from boot
    clear the SWGEN0 latch              so the next assertion makes an edge
    rearm PCI interrupt delivery        the walk does not do it under
                                        pre-Hopper MSI
    register the threaded IRQ handler   nothing can reach it yet
    enable the GSP vector at its leaf   deliveries become possible here
    drain the GSP-to-CPU queue          messages posted before the clear

Clearing the latch makes the first interrupt possible. Messages the GSP posted
before that clear produce no interrupt, so the queue drain follows.

The tree is quiesced before the handler is registered. Registering unmasks the
PCI interrupt, and a leaf enable that boot left set would reach a handler that
services one vector and has no way to service any other. Open RM clears all
leaf enables at the same point for the same reason.

The latch is cleared after the tree walk, not before. The walk erases every leaf
bit, so a message posted between an earlier clear and the walk would leave the
latch set with nothing in the tree to show for it, and on Turing no later
message would signal the tree at all. Clearing last can instead leave the GSP
vector pending with the latch already clear, so enabling the vector delivers one
interrupt whose ``IRQSTAT`` reads zero. The queue drain that follows reads the
message.

The GSP event vector
--------------------

The GSP event uses a fixed vector, ``GSP_INTR_0_VECTOR`` (155), on Turing
through Blackwell. Vector 155 is leaf 4, bit 27, subtree 2. nova-core enables
that leaf bit and services it, with no runtime vector discovery.

A full unit-to-vector table can be fetched from the GSP by RPC. nova-core does
not fetch it, because a pinned vector needs no lookup.

Draining and classifying the GSP-to-CPU queue
=============================================

The queue carries both command replies and unsolicited events. Each message is
routed by its function code and its RPC sequence number, into one of three
classes:

* Function code and sequence both match the awaited reply. The message is
  decoded and returned to the caller that sent the command.
* The function code matches but the sequence does not. This is a reply to a
  command that already timed out, so it is logged at warning level and dropped
  rather than satisfying a later command that reused the same function code.
* Anything else is an unsolicited event. OS-error and robust-channel records are
  logged at error level. An unrecognized function code is logged at warning
  level. Other known events (GSP logs, libos prints, assertion records,
  lifecycle notices) need no action and are not logged again, because the RPC
  receive trace already records their arrival.

The read pointer advances past the message in all three cases, and also when a
matched message fails to decode, so a message is never left at the queue head
for the next receive to parse again.

Corrupt framing is the exception. A message carries its length inside the
region the checksum covers, so once the framing or the checksum fails there is
no trustworthy length with which to skip the message. Such a failure poisons the
queue and every later receive fails, which the IRQ thread reports before
disabling the GSP vector.

The classifier is a fixed set of function codes rather than a handler registry.
The events that need action are handled directly in it.

Both the polling path and the IRQ thread route messages through this classifier
under the command-queue lock. Replies and events share one queue and one set of
read pointers, so one lock covers the whole drain. A thread waiting for a reply
dispatches any event it reads first and keeps waiting, under a single deadline
for the whole wait rather than a fresh timeout after each message.

One lock means a drain waits for an in-flight command's receive to finish or
time out. For log and error records that delay does not matter.

Design notes
============

Register naming
---------------

nova-core uses the ``NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_*`` names for the CPU
tree on both pre-Hopper and Hopper-plus parts. Any function reaches its own tree
through that aperture. The Hopper-plus central aperture (``NV_GIN_CPU_INTR_*``)
configures other functions and is not used by the CPU path.

The controller has two names in the hardware headers and in Open RM.
``NV_CTRL`` names the tree on pre-Hopper parts, and ``NV_GIN`` names the
Hopper+ unit that contains the tree along with arbiter logic. This document
calls the controller GIN throughout, because the tree nova-core drives is the
same on every supported part.

Tree API
--------

Servicing a leaf has a required order: read its pending bits, then clear them.
Reading a leaf is what produces the handle that clears it, so clearing a leaf
before reading it does not compile. Enabling and disabling a vector or a subtree
takes no such order, so those are operations on the tree itself.

The handle orders the calls that service one leaf. It is not a lock and it does
not coordinate the tree as a whole. Nothing stops two walks from running against
the tree at once. nova-core does not run concurrent walks: the GSP event handler
touches only its own leaf and never walks the tree, and the only whole-tree
walk, the startup drain, runs once during probe.

Threaded handler
----------------

The drain sleeps: it takes the command-queue mutex and walks shared memory, so it
cannot run in hard-IRQ context. nova-core uses a threaded IRQ handler. The top
half clears the GIN leaf, takes every cause the falcon reports, rearms delivery,
and wakes the IRQ thread if SWGEN0 was among them. The thread takes the lock and
drains the queue. The self-test does no sleeping work and uses a non-threaded
handler with a completion.

Shared BAR0 mapping
-------------------

The GPU, the self-test, and the GSP event handler read the same BAR0 registers.
nova-core keeps one BAR0 mapping and lets each of them borrow it. An interrupt
handler is torn down when the device unbinds, so it only runs while the mapping
is alive.

Self-test
=========

The self-test runs during driver probe. It registers a real interrupt handler
and confirms that an interrupt injected at the GPU is delivered all the way to
that handler, so it needs a working GPU and PCI interrupt path. It is gated by
``CONFIG_NOVA_CORE_IRQ_SELFTEST`` and runs before GSP boot, so it never touches
GSP interrupt state.

The parts with no hardware dependency are covered by KUnit tests instead: the
vector encoding, the subtree and leaf arithmetic, and the per-architecture rearm
policy.

The test drives ``LEAF_TRIGGER``, a hardware register that every supported part
implements. Writing a vector number to it latches that vector exactly as its
unit would, after which the vector takes the ordinary path to the CPU under the
ordinary enables.

The test drives vector 129, at leaf 4 bit 1. It registers a handler for that
vector and triggers it twice, waiting for the first delivery before triggering
the second. Its handler deliberately mirrors the notification path: it clears
only its own leaf bit and rearms PCI interrupt delivery, rather than walking the
tree.

The two interrupts cannot coalesce into one, because the second is triggered
only after the first handler has finished. A handler that fails to rearm times
out on the second delivery instead of passing. A single delivery serviced by a
full tree walk cannot detect that, because the walk's own TOP_EN restore
produces an edge by itself.

The test passes only if both deliveries arrive, each one finds the doorbell bit
and nothing else pending in the leaf, and the leaf is clear once the source is
stopped. Anything else fails probe. Requiring the exact mask on the second
delivery shows that the first handler's clear reached the hardware. The test
runs before GSP boot on a leaf the drain has just cleared, so no other vector in
that leaf can be active and the exact mask costs nothing.

The test borrows the allocation that probe made for the serviced subtrees rather
than allocating its own, and looks up the vector for the doorbell's own subtree.
A doorbell vector moved to a subtree nova-core does not service fails that
lookup, and with it the self-test and probe, rather than being misrouted
silently.

The test exercises the interrupt path from the GPU to the handler without GSP
firmware, which is useful when bringing up PCI, MSI, MSI-X, and passthrough
setups. Under MSI-X a pass also shows that the per-subtree table entry routing
works, since the delivery arrives on the entry belonging to the serviced
subtree.

Virtualization
==============

The per-function trees, the GFID routing, and the central ``NV_GIN`` aperture
support virtualization: each VF gets its own tree, and the PF or firmware routes
a unit's interrupt to the right function. MIG (multi-instance GPU) partitioning
adds more structure. nova-core services the CPU tree of one function, and
implements no VF tree management, GFID routing, or MIG support.

References
==========

* nova-core source: the register definitions in ``regs.rs``, the interrupt HAL
  and tree API in the ``irq`` module, and the GSP command queue in the ``gsp``
  module.
