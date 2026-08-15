// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! GSP event (SWGEN0) interrupt handling.
//!
//! The GSP firmware raises SWGEN0 when it has posted messages in the GSP-to-CPU queue. That
//! signal reaches the CPU as a PCI interrupt through the GIN tree. This module provides the
//! threaded IRQ handler for it. The top half services the GIN leaf and the falcon SWGEN0 latch,
//! and the IRQ thread drains the message queue.
//!
//! See `Documentation/gpu/nova/core/interrupts.rst`.

use kernel::{
    device, irq, pci,
    prelude::*,
    sync::{
        aref::ARef,
        Arc, //
    },
};

use super::interrupt_tree::{
    LeafIndex,
    Tree, //
};
use crate::{
    driver::Bar0,
    falcon::gsp::Gsp as GspFalcon,
    gpu::Chipset,
    gsp::cmdq::Cmdq, //
};

/// Fixed GSP notification vector.
///
/// The resource manager pins the GSP SWGEN0 notification to this vector on every supported chip,
/// so nova-core uses the constant directly instead of discovering it at runtime. The leaf and bit
/// serviced by the handler are derived from it.
pub(crate) const GSP_INTR_0_VECTOR: u32 = 155;

/// Leaf and bit index of the GSP notification vector within the interrupt tree.
const GSP_LOC: (usize, u32) = super::interrupt_tree::vector_leaf_bit(GSP_INTR_0_VECTOR);

/// Leaf holding the GSP notification vector.
const GSP_LEAF: usize = GSP_LOC.0;

/// Bit of the GSP notification vector within its leaf.
const GSP_BIT: u32 = 1 << GSP_LOC.1;

/// Subtree carrying the GSP notification vector, and the only subtree nova-core services.
///
/// Probe allocates PCI vectors for this subtree, and the GSP handler names it as the subtree it
/// serves, both when it takes its vector and when it rearms.
pub(crate) const GSP_SUBTREE: u32 = super::interrupt_tree::vector_subtree_mask(GSP_INTR_0_VECTOR);

/// Clears the interrupt state that GSP boot left behind.
///
/// Disables every vector in every implemented leaf, clears the tree's pending bits, clears the
/// falcon's SWGEN0 latch, and rearms PCI interrupt delivery. On return no vector is enabled, so
/// the tree delivers nothing.
pub(crate) fn quiesce(bar: Bar0<'_>, chipset: Chipset, irq_type: pci::IrqType) {
    let tree = Tree::new(chipset, irq_type, GSP_SUBTREE);
    tree.disable_all_leaves(bar);
    tree.drain(bar);
    // GSP boot consumes its notifications by polling the queue, which leaves SWGEN0 latched, and
    // the GSP drives no new signal while it is set. Clear it after the tree drain, which erases
    // every leaf bit and would erase the one a message posted since the clear had set.
    GspFalcon::clear_swgen0_intr(bar);
    // The `TOP_EN` cycle in `drain` is the rearm for the two enable-cycle methods, but pre-Hopper
    // MSI rearms through a configuration-space write instead. An interrupt delivered before probe
    // leaves delivery un-armed on that path, with no handler to have rearmed it.
    tree.rearm_pci_irq(bar, GSP_SUBTREE);
}

/// Enables the GSP notification vector at its leaf.
///
/// The GSP interrupt is delivered from this point on.
pub(crate) fn enable(bar: Bar0<'_>, chipset: Chipset, irq_type: pci::IrqType) {
    let tree = Tree::new(chipset, irq_type, GSP_SUBTREE);
    // A message posted after `quiesce` latches this leaf bit while the vector is still disabled,
    // so enabling the vector raises that interrupt rather than losing the message.
    tree.leaf(LeafIndex::new::<GSP_LEAF>()).enable(bar, GSP_BIT);
}

/// Threaded IRQ handler for the GSP SWGEN0 event.
///
/// The top half clears the GIN leaf and reads the falcon SWGEN0 latch. The IRQ thread drains the
/// GSP-to-CPU message queue, which takes the command-queue lock.
#[pin_data]
pub(crate) struct GspInterrupt<'a> {
    /// Borrowed BAR0, for GIN and falcon register access from interrupt context.
    bar: Bar0<'a>,
    /// The GSP command queue, drained by the IRQ thread.
    cmdq: Arc<Cmdq>,
    /// The GIN interrupt tree for this chipset.
    tree: Tree,
    /// Chipset, for the falcon retrigger, which Turing does not implement.
    chipset: Chipset,
    /// Device, for logging from interrupt context without taking the command-queue lock.
    dev: ARef<device::Device>,
}

impl<'a> GspInterrupt<'a> {
    /// Creates the handler for `chipset`, borrowing `bar` and sharing `cmdq` with the rest of the
    /// driver.
    pub(crate) fn new(
        bar: Bar0<'a>,
        cmdq: Arc<Cmdq>,
        chipset: Chipset,
        irq_type: pci::IrqType,
        dev: ARef<device::Device>,
    ) -> impl PinInit<Self, Error> + 'a {
        try_pin_init!(Self {
            bar,
            cmdq,
            tree: Tree::new(chipset, irq_type, GSP_SUBTREE),
            chipset,
            dev,
        }? Error)
    }
}

impl irq::ThreadedHandler for GspInterrupt<'_> {
    /// Top half: clears the GIN leaf, takes every cause the falcon reports, and rearms PCI
    /// interrupt delivery.
    fn handle(&self) -> irq::ThreadedIrqReturn {
        let bar = self.bar;

        // Only service our own vector: require the GSP bit in the leaf and clear just that bit, so
        // a co-pending vector in the same leaf stays pending for whoever services it. The subtree
        // stays enabled, so there is no whole-tree disable and enable.
        let leaf = self
            .tree
            .leaf(LeafIndex::new::<GSP_LEAF>())
            .read_pending(bar);
        if leaf.pending_bits() & GSP_BIT == 0 {
            // Nothing to service, but nova-core is the only consumer of this PCI interrupt, so
            // skipping the rearm here would silence every later interrupt as well.
            self.tree.rearm_pci_irq(bar, GSP_SUBTREE);
            return irq::ThreadedIrqReturn::None;
        }
        leaf.clear_vectors(bar, GSP_BIT);

        let status = GspFalcon::take_swgen0_intr(bar);

        // Every cause the falcon reports leaves the falcon's enabled set on this invocation. A
        // cause left latched holds that set non-empty, and the falcon signals the tree only on a
        // transition of the set, so no later SWGEN0 would signal at all.
        let unserviceable = status.with_swgen0(false);
        if unserviceable.into_raw() != 0 {
            // The tree routes every falcon cause to this vector, so a cause other than a posted
            // message also arrives here, for example a HALT from a GSP crash. nova-core has no
            // recovery path for those, so report the status rather than discarding it, then mask
            // the cause.
            dev_err!(
                &self.dev,
                "unserviceable GSP falcon interrupt, IRQSTAT {:#x}\n",
                status.into_raw()
            );
            GspFalcon::mask_and_clear_intr(bar, unserviceable);
        }

        // The leaf clear above consumed the tree's record of this interrupt, and the falcon signals
        // the tree only on a transition of its enabled causes, so a cause that arrived while this
        // handler ran would never reach the CPU. Re-emit to supply that transition.
        GspFalcon::retrigger_intr(bar, self.chipset);

        // Delivery resumes only after this, so it must happen on every path that services the
        // vector, including the fault path above.
        self.tree.rearm_pci_irq(bar, GSP_SUBTREE);

        // SWGEN0 is the message-queue notification, so wake the IRQ thread to drain it.
        if status.swgen0() {
            irq::ThreadedIrqReturn::WakeThread
        } else {
            irq::ThreadedIrqReturn::Handled
        }
    }

    /// IRQ thread: drains and dispatches the GSP-to-CPU message queue.
    fn handle_threaded(&self) -> irq::IrqReturn {
        if let Err(e) = self.cmdq.drain(self.bar) {
            // A queue that fails to drain cannot advance past the message that failed, so every
            // later notification would repeat this failure. Disable the source instead.
            self.tree
                .leaf(LeafIndex::new::<GSP_LEAF>())
                .disable(self.bar, GSP_BIT);
            dev_err!(
                &self.dev,
                "GSP event drain failed ({:?}), the message queue is no longer serviced\n",
                e
            );
        }
        irq::IrqReturn::Handled
    }
}

/// The registered GSP event interrupt.
///
/// Wraps the threaded IRQ registration so that teardown disables the GSP source at the interrupt
/// tree before `free_irq` runs. This closes the window, including a probe partial-unwind, in which
/// an interrupt could be delivered to a handler that is being freed.
#[pin_data(PinnedDrop)]
pub(crate) struct GspIrq<'a> {
    #[pin]
    reg: irq::ThreadedRegistration<'a, GspInterrupt<'a>>,
    /// Borrowed BAR0 and the interrupt tree, used by the teardown to disable the GSP source.
    bar: Bar0<'a>,
    tree: Tree,
}

impl<'a> GspIrq<'a> {
    /// Registers the GSP SWGEN0 threaded handler for the GSP subtree in `vectors`.
    ///
    /// # Safety
    ///
    /// The caller must not leak the returned value: its [`Drop`] runs `free_irq`.
    pub(crate) unsafe fn new(
        pdev: &'a pci::Device<device::Bound>,
        vectors: &'a super::SubtreeVectors<'a>,
        bar: Bar0<'a>,
        cmdq: Arc<Cmdq>,
        chipset: Chipset,
    ) -> impl PinInit<Self, Error> + 'a {
        let dev: ARef<device::Device> = pdev.as_ref().into();
        try_pin_init!(Self {
            // SAFETY: the caller guarantees the returned `GspIrq` is not leaked, so this
            // registration's `Drop` (`free_irq`) always runs.
            reg <- unsafe {
                irq::ThreadedRegistration::new(
                    vectors.request_for(GSP_SUBTREE)?,
                    irq::Flags::TRIGGER_NONE,
                    c"nova-core",
                    GspInterrupt::new(bar, cmdq, chipset, vectors.irq_type(), dev),
                )
            },
            bar,
            tree: Tree::new(chipset, vectors.irq_type(), GSP_SUBTREE),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for GspIrq<'_> {
    fn drop(self: Pin<&mut Self>) {
        // Disable the GSP source before `reg` drops and runs `free_irq`, so no interrupt reaches a
        // handler being torn down. This `PinnedDrop` runs before any field drops, so the order is
        // disable-then-free_irq.
        let this = self.project();
        this.tree
            .leaf(LeafIndex::new::<GSP_LEAF>())
            .disable(this.bar, GSP_BIT);
    }
}
