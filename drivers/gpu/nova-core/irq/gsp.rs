// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! GSP event (SWGEN0) interrupt handling.
//!
//! The GSP firmware raises SWGEN0 when it has posted messages in the GSP-to-CPU queue. That
//! signal reaches the CPU as a PCI interrupt through the GIN tree. This module provides the
//! threaded IRQ handler for it. The top half services the GIN leaf and the falcon's latched
//! causes, and the IRQ thread drains the message queue.
//!
//! See `Documentation/gpu/nova/core/interrupts.rst`.

use kernel::{
    device,
    irq,
    pci,
    prelude::*, //
};

use super::{
    interrupt_tree::{
        GinVector,
        LeafEnableGuard,
        Subtree,
        TopEnableGuard,
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
/// GSP-RM pins the GSP SWGEN0 notification to this vector on every supported chip, so nova-core
/// uses the constant directly instead of discovering it at runtime. The leaf and bit serviced by
/// the handler are derived from it.
const GSP_INTR_0_VECTOR: GinVector = GinVector::new::<155>();

/// Subtree carrying the GSP notification vector, and the only subtree nova-core services.
///
/// Probe allocates PCI vectors for this subtree, and the GSP handler names it as the subtree it
/// serves, both when it takes its vector and when it rearms.
pub(crate) const GSP_SUBTREE: Subtree = GSP_INTR_0_VECTOR.subtree();

/// Clears the interrupt state that GSP boot left behind.
///
/// Resets the tree `vectors` covers, then clears the falcon's SWGEN0 latch. On return no vector
/// is enabled, so the tree delivers nothing.
///
/// # Errors
///
/// `EINVAL` if this architecture does not implement a subtree `vectors` services.
pub(crate) fn quiesce(bar: Bar0<'_>, chipset: Chipset, vectors: &SubtreeVectors<'_>) -> Result {
    vectors.reset_tree(bar, chipset)?;
    // GSP boot consumes its notifications by polling the queue, which leaves SWGEN0 latched, and
    // the GSP drives no new signal while it is set. The clear comes after the tree reset, which
    // erases every leaf bit and would erase the one a message posted since the clear had set.
    GspFalcon::clear_swgen0_intr(bar);

    Ok(())
}

/// Threaded IRQ handler for the GSP SWGEN0 event.
///
/// The top half clears the GIN leaf and takes the falcon causes pending for the host. The IRQ
/// thread drains the GSP-to-CPU message queue, which takes the command-queue lock.
pub(crate) struct GspInterrupt<'a> {
    /// Borrowed BAR0, for falcon register access from interrupt context.
    bar: Bar0<'a>,
    /// The GSP command queue, drained by the IRQ thread.
    cmdq: &'a Cmdq,
    /// The GIN interrupt tree for this chipset.
    tree: Tree<'a>,
    /// Chipset, for the falcon retrigger and the routing registers, which both differ by
    /// architecture.
    chipset: Chipset,
    /// Device, for logging from interrupt context without taking the command-queue lock.
    dev: &'a device::Device,
}

impl<'a> GspInterrupt<'a> {
    /// Creates the handler for `chipset`, borrowing `bar` and `cmdq` from the rest of the driver.
    fn new(
        bar: Bar0<'a>,
        cmdq: &'a Cmdq,
        tree: Tree<'a>,
        chipset: Chipset,
        dev: &'a device::Device,
    ) -> Self {
        Self {
            bar,
            cmdq,
            tree,
            chipset,
            dev,
        }
    }
}

impl irq::ThreadedHandler for GspInterrupt<'_> {
    /// Top half: clears the GIN leaf, takes every falcon cause pending for the host, and rearms
    /// PCI interrupt delivery.
    fn handle(&self) -> irq::ThreadedIrqReturn {
        let bar = self.bar;

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

        let status = GspFalcon::take_host_intr(bar, self.chipset);

        // A cause left latched holds the falcon's host-routed set non-empty, and the falcon
        // signals the tree only on a transition of that set, so no later SWGEN0 would signal.
        let unserviceable = status.with_swgen0(false);
        if unserviceable.into_raw() != 0 {
            // nova-core has no recovery path for a cause other than a posted message, for example
            // a HALT from a GSP crash, so report it rather than discarding it.
            dev_err!(
                &self.dev,
                "unserviceable GSP falcon interrupt, IRQSTAT {:#x}\n",
                status.into_raw()
            );
            GspFalcon::clear_intr(bar, unserviceable);
        }

        // The leaf clear above consumed the tree's record of this interrupt, and the falcon signals
        // the tree only on a transition of its host-routed causes, so a cause that arrived while
        // this handler ran would never reach the CPU. Re-emit to supply that transition.
        GspFalcon::retrigger_intr(bar, self.chipset);

        // Delivery resumes only after this, so it must happen on every path that services the
        // vector, including the fault path above.
        self.tree.rearm_pci_irq(GSP_SUBTREE);

        // SWGEN0 is the message-queue notification, so wake the IRQ thread to drain it.
        if status.swgen0() {
            irq::ThreadedIrqReturn::WakeThread
        } else {
            irq::ThreadedIrqReturn::Handled
        }
    }

    /// IRQ thread: drains the GSP-to-CPU message queue.
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
/// The fields tear down in declaration order, which is the order this needs: disabling the leaf
/// stops new deliveries, `free_irq` then waits for a handler still in flight, and only then is
/// the subtree disabled at `TOP`, so a late handler cannot rearm it.
#[pin_data]
pub(crate) struct GspIrq<'a> {
    _leaf_guard: LeafEnableGuard<'a>,
    #[pin]
    reg: irq::ThreadedRegistration<'a, GspInterrupt<'a>>,
    _top_guard: TopEnableGuard<'a>,
}

impl<'a> GspIrq<'a> {
    /// Registers the GSP SWGEN0 threaded handler for the GSP subtree in `vectors`, then enables
    /// the subtree and the GSP notification vector.
    ///
    /// # Errors
    ///
    /// `EINVAL` if this architecture does not implement the subtree carrying the GSP
    /// notification, or if `vectors` does not service it.
    ///
    /// # Safety
    ///
    /// The caller must not leak the returned value: its [`Drop`] runs `free_irq`.
    pub(crate) unsafe fn new(
        pdev: &'a pci::Device<device::Bound>,
        vectors: &'a SubtreeVectors<'a>,
        bar: Bar0<'a>,
        cmdq: &'a Cmdq,
        chipset: Chipset,
    ) -> impl PinInit<Self, Error> + 'a {
        let dev = pdev.as_ref();

        // The fields below are initialized in the opposite order to the one they are declared in,
        // so that the handler is registered before anything it serves is enabled.
        try_pin_init!(Self {
            // SAFETY: the caller guarantees the returned `GspIrq` is not leaked, so this
            // registration's `Drop` (`free_irq`) always runs.
            reg <- unsafe {
                irq::ThreadedRegistration::new(
                    vectors.request_for(GSP_SUBTREE)?,
                    irq::Flags::TRIGGER_NONE,
                    c"nova-core",
                    Ok(GspInterrupt::new(
                        bar,
                        cmdq,
                        vectors.tree(bar, chipset)?,
                        chipset,
                        dev,
                    )),
                )
            },
            // Under pre-Hopper MSI the rearm is a configuration-space write, so nothing else
            // restores the `TOP` enables that the tree reset cleared.
            _top_guard: reg.handler().tree.enable_top_guarded(),
            // A message posted during `quiesce` latches this leaf bit while the vector is still
            // disabled, so enabling it raises that interrupt rather than losing the message.
            _leaf_guard: reg.handler().tree.enable_leaf_guarded(
                GSP_INTR_0_VECTOR.leaf_index(),
                GSP_INTR_0_VECTOR.leaf_mask(),
            ),
        })
    }
}
