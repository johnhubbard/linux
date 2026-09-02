// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! GSP event (SWGEN0) interrupt handling.
//!
//! The GSP firmware raises SWGEN0 when it has posted messages in the GSP-to-CPU queue. That
//! signal reaches the CPU as a PCI interrupt through the GIN tree. This module provides the
//! threaded IRQ handler for it. The top half services the GIN leaf and the falcon's latched
//! causes, and the IRQ thread drains the message queue.
//!
//! See "The GSP event" in `Documentation/gpu/nova/core/interrupts.rst`.

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
    gsp::cmdq::Cmdq,
    regs, //
};

/// Fixed GSP SWGEN0 notification vector, pinned by GSP firmware on every supported chip.
const GSP_INTR_0_VECTOR: GinVector = GinVector::new::<155>();

/// Subtree carrying the GSP notification vector, and the only subtree nova-core services.
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
    // Ordered after the tree reset, which would otherwise erase the leaf bit that a message
    // posted in between had set. See "Enabling the GSP event" in interrupts.rst.
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
    /// Creates the handler for `chipset`.
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

    /// Reports and clears every host-routed falcon cause in `status` other than SWGEN0.
    ///
    /// `status` must come from [`GspFalcon::take_host_intr`]. Returns the causes still set after
    /// the clear, which is empty when `status` carried none.
    fn clear_faults(
        &self,
        status: regs::NV_PFALCON_FALCON_IRQSTAT,
    ) -> regs::NV_PFALCON_FALCON_IRQSTAT {
        let faults = status.with_swgen0(false);
        if faults.into_raw() == 0 {
            return faults;
        }

        dev_err!(
            &self.dev,
            "unserviceable GSP falcon interrupt, IRQSTAT {:#x}\n",
            status.into_raw()
        );
        GspFalcon::clear_intr(self.bar, faults);

        GspFalcon::read_host_intr(self.bar, self.chipset).with_swgen0(false)
    }
}

impl irq::ThreadedHandler for GspInterrupt<'_> {
    /// Top half: services the GSP notification vector and rearms PCI interrupt delivery.
    fn handle(&self) -> irq::ThreadedIrqReturn {
        let bar = self.bar;

        // Service only the GSP vector, so a vector pending alongside it in this leaf stays
        // pending for whoever services it.
        let leaf = self.tree.read_pending(GSP_INTR_0_VECTOR.leaf_index());
        if !leaf.vectors().contains(GSP_INTR_0_VECTOR.leaf_mask()) {
            // Nothing to service, but delivery still has to be rearmed.
            self.tree.rearm_pci_irq(GSP_SUBTREE);
            return irq::ThreadedIrqReturn::None;
        }
        leaf.clear_vectors(GSP_INTR_0_VECTOR.leaf_mask());

        let status = GspFalcon::take_host_intr(bar, self.chipset);

        let remaining_faults = self.clear_faults(status);
        if remaining_faults.into_raw() == 0 {
            // Every host cause is clear, so the falcon can signal the tree again.
            GspFalcon::retrigger_intr(bar, self.chipset);
        } else {
            // A retrigger would deliver this vector again at once, without end. Disabling it
            // loses no notification. See "Retriggering a falcon" in interrupts.rst.
            self.tree.disable_leaf(
                GSP_INTR_0_VECTOR.leaf_index(),
                GSP_INTR_0_VECTOR.leaf_mask(),
            );
            dev_err!(
                &self.dev,
                "GSP falcon cause {:#x} needs a device reset, GSP events are no longer serviced\n",
                remaining_faults.into_raw()
            );
        }

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
            // A failed drain repeats on every later notification, so stop delivering them.
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
/// The fields tear down in declaration order, which is required: disabling the leaf stops new
/// deliveries, `free_irq` then waits out a handler still in flight, and the subtree is disabled
/// at `TOP` last, so a late handler cannot rearm it.
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
        // so the handler is registered before anything it serves is enabled.
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
            _top_guard: reg.handler().tree.enable_top_guarded(),
            // A message posted while the vector was disabled stays latched in the leaf, so
            // enabling it delivers that interrupt.
            _leaf_guard: reg.handler().tree.enable_leaf_guarded(
                GSP_INTR_0_VECTOR.leaf_index(),
                GSP_INTR_0_VECTOR.leaf_mask(),
            ),
        })
    }
}
