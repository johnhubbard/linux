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

use super::{
    interrupt_tree::{
        GinVector,
        LeafEnableGuard,
        Subtree,
        Tree, //
    },
    SubtreeVectors, //
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
const GSP_INTR_0_VECTOR: GinVector = GinVector::new::<155>();

/// Subtree carrying the GSP notification vector, and the only subtree nova-core services.
///
/// Probe allocates PCI vectors for this subtree, and the GSP handler names it as the subtree it
/// serves, both when it takes its vector and when it rearms.
pub(crate) const GSP_SUBTREE: Subtree = GSP_INTR_0_VECTOR.subtree();

/// Clears the interrupt state that GSP boot left behind.
///
/// Disables every vector in every implemented leaf, clears the falcon's SWGEN0 latch, clears the
/// tree's pending bits, and rearms PCI interrupt delivery. On return no vector is enabled, so the
/// tree delivers nothing.
pub(crate) fn quiesce(bar: Bar0<'_>, chipset: Chipset, irq_type: pci::IrqType) {
    let tree = Tree::new(bar, chipset, irq_type, GSP_SUBTREE.into());
    tree.disable_all_leaves();
    // GSP boot consumes its notifications by polling the queue, which leaves SWGEN0 latched.
    // Clear it before the tree drain below, so the drain clears the tree state the clear sets.
    // Messages already posted raise no interrupt of their own, and the caller's queue drain
    // covers them.
    GspFalcon::clear_swgen0_intr(bar);
    tree.drain();
    // The `TOP_EN` cycle in `drain` is the rearm for the two enable-cycle methods, but pre-Hopper
    // MSI rearms through a configuration-space write instead. An interrupt delivered before probe
    // leaves delivery un-armed on that path, with no handler to have rearmed it.
    tree.rearm_pci_irq(GSP_SUBTREE);
}

/// Threaded IRQ handler for the GSP SWGEN0 event.
///
/// The top half clears the GIN leaf and reads the falcon SWGEN0 latch. The IRQ thread drains the
/// GSP-to-CPU message queue, which takes the command-queue lock.
#[pin_data]
pub(crate) struct GspInterrupt<'a> {
    /// Borrowed BAR0, for falcon register access from interrupt context.
    bar: Bar0<'a>,
    /// The GSP command queue, drained by the IRQ thread.
    cmdq: Arc<Cmdq>,
    /// The GIN interrupt tree for this chipset.
    tree: Tree<'a>,
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
            tree: Tree::new(bar, chipset, irq_type, GSP_SUBTREE.into()),
            dev,
        }? Error)
    }
}

impl irq::ThreadedHandler for GspInterrupt<'_> {
    /// Top half: clears the GIN leaf, takes the falcon SWGEN0 latch, and rearms PCI interrupt
    /// delivery.
    fn handle(&self) -> irq::ThreadedIrqReturn {
        // Only service our own vector: require the GSP bit in the leaf and clear just that bit, so
        // a co-pending vector in the same leaf stays pending for whoever services it. The subtree
        // stays enabled, so there is no whole-tree disable and enable.
        let leaf = self.tree.read_pending(GSP_INTR_0_VECTOR.leaf_index());
        if !leaf.vectors().contains(GSP_INTR_0_VECTOR.leaf_mask()) {
            // Nothing to service, but nova-core is the only consumer of this PCI interrupt, so
            // skipping the rearm here would silence every later interrupt as well.
            self.tree.rearm_pci_irq(GSP_SUBTREE);
            return irq::ThreadedIrqReturn::None;
        }
        leaf.clear_vectors(GSP_INTR_0_VECTOR.leaf_mask());

        // SWGEN0 is the message-queue notification, so wake the IRQ thread to drain it.
        let status = GspFalcon::take_swgen0_intr(self.bar);
        let ret = if status.swgen0() {
            irq::ThreadedIrqReturn::WakeThread
        } else {
            // The tree routes every falcon cause to this vector, so something other than a posted
            // message fired it, for example a HALT from a GSP crash. There is no recovery path for
            // those causes, so report the status rather than discarding it.
            dev_err!(
                &self.dev,
                "GSP interrupt with no SWGEN0, falcon IRQSTAT {:#x}\n",
                status.into_raw()
            );
            irq::ThreadedIrqReturn::Handled
        };

        // Delivery resumes only after this, so it must happen on every path that services the
        // vector, including the fault path above.
        self.tree.rearm_pci_irq(GSP_SUBTREE);

        ret
    }

    /// IRQ thread: drains and dispatches the GSP-to-CPU message queue.
    fn handle_threaded(&self) -> irq::IrqReturn {
        if let Err(e) = self.cmdq.drain() {
            // A queue that fails to drain cannot advance past the message that failed, so every
            // later notification would repeat this failure. Disable the source instead.
            self.tree.disable_leaf(
                GSP_INTR_0_VECTOR.leaf_index(),
                GSP_INTR_0_VECTOR.leaf_mask(),
            );
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
/// The fields tear down in declaration order, which is the order this needs: dropping the guard
/// disables the GSP vector, and only then does `reg` drop and run `free_irq`. That closes the
/// window, including a probe partial-unwind, in which an interrupt could be delivered to a handler
/// being freed.
#[pin_data]
pub(crate) struct GspIrq<'a> {
    _leaf_guard: LeafEnableGuard<'a>,
    #[pin]
    reg: irq::ThreadedRegistration<'a, GspInterrupt<'a>>,
}

impl<'a> GspIrq<'a> {
    /// Registers the GSP SWGEN0 threaded handler for the GSP subtree in `vectors`, then enables
    /// the GSP notification vector.
    ///
    /// # Safety
    ///
    /// The caller must not leak the returned value: its [`Drop`] runs `free_irq`.
    pub(crate) unsafe fn new(
        pdev: &'a pci::Device<device::Bound>,
        vectors: &'a SubtreeVectors<'a>,
        bar: Bar0<'a>,
        cmdq: Arc<Cmdq>,
        chipset: Chipset,
    ) -> impl PinInit<Self, Error> + 'a {
        let dev: ARef<device::Device> = pdev.as_ref().into();
        let tree = Tree::new(bar, chipset, vectors.irq_type(), GSP_SUBTREE.into());

        // The fields below are initialized in the opposite order to the one they are declared in,
        // so that the handler is registered before the vector it serves is enabled.
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
            // A message posted during `quiesce` latches this leaf bit while the vector is still
            // disabled, so enabling it raises that interrupt rather than losing the message.
            _leaf_guard: tree.enable_leaf_guarded(
                GSP_INTR_0_VECTOR.leaf_index(),
                GSP_INTR_0_VECTOR.leaf_mask(),
            ),
        })
    }
}
