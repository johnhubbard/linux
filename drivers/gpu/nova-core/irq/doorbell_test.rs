// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Interrupt delivery self-test, driven through the CPU doorbell vector.
//!
//! Injects a known vector through the GIN software trigger twice, one delivery at a time, and
//! confirms both reach the handler. Gated behind `CONFIG_NOVA_CORE_IRQ_SELFTEST` and run before
//! GSP boot, so it never reads or clears GSP interrupt state.
//!
//! See "Self-test" in `Documentation/gpu/nova/core/interrupts.rst`.

use core::pin::Pin;

use kernel::{
    device::Bound,
    irq,
    pci,
    prelude::*,
    sync::{
        atomic::{
            Atomic,
            Relaxed, //
        },
        Completion, //
    },
    time, //
};

use super::{
    interrupt_tree::{
        GinVector,
        LeafEnableGuard,
        LeafMask,
        Subtree,
        TopEnableGuard,
        Tree, //
    },
    SubtreeVectors, //
};

use crate::{
    driver::Bar0,
    gpu::Chipset, //
};

/// Fixed CPU doorbell vector, pinned by GSP firmware on every supported chip.
const DOORBELL_VECTOR: GinVector = GinVector::new::<129>();

/// Subtree carrying the doorbell vector, and the only subtree this test services.
const DOORBELL_SUBTREE: Subtree = DOORBELL_VECTOR.subtree();

/// Time allowed for each of the two deliveries to arrive.
const DELIVERY_TIMEOUT_MS: time::Msecs = 1000;

/// Interrupt handler installed by the self-test.
///
/// Clears its own leaf bit and rearms PCI interrupt delivery, leaving the rest of the tree
/// untouched, which is what a notification handler does. Records the leaf's pending bits from
/// each of the first two deliveries and signals the matching completion.
#[pin_data]
struct DoorbellTestHandler<'a> {
    /// The interrupt tree, which carries the borrowed BAR0 that register access needs.
    tree: Tree<'a>,
    /// Signaled by the first delivery.
    #[pin]
    first: Completion,
    /// Signaled by the second delivery.
    #[pin]
    second: Completion,
    /// Count of deliveries this handler has serviced.
    irq_count: Atomic<u32>,
    /// Doorbell leaf's pending bits observed on the first delivery.
    first_pending: Atomic<u32>,
    /// Doorbell leaf's pending bits observed on the second delivery.
    second_pending: Atomic<u32>,
}

impl irq::Handler for DoorbellTestHandler<'_> {
    fn handle(&self) -> irq::IrqReturn {
        // Clear only this handler's own bit and leave `TOP_EN` alone, so a missing rearm shows up
        // as a missing delivery.
        let leaf = self.tree.read_pending(DOORBELL_VECTOR.leaf_index());
        let pending = leaf.vectors();
        if !pending.contains(DOORBELL_VECTOR.leaf_mask()) {
            self.tree.rearm_pci_irq(DOORBELL_SUBTREE);
            return irq::IrqReturn::None;
        }
        leaf.clear_vectors(DOORBELL_VECTOR.leaf_mask());

        let count = self.irq_count.fetch_add(1, Relaxed);

        // Rearm before signaling, so delivery is possible again by the time the waiting thread
        // triggers the next vector.
        self.tree.rearm_pci_irq(DOORBELL_SUBTREE);

        match count {
            0 => {
                self.first_pending.store(pending.into_raw(), Relaxed);
                self.first.complete_all();
            }
            1 => {
                self.second_pending.store(pending.into_raw(), Relaxed);
                self.second.complete_all();
            }
            _ => (),
        }

        irq::IrqReturn::Handled
    }
}

/// Everything the running self-test owns.
///
/// The fields tear down in declaration order, which is required: the leaf enable goes first so no
/// new delivery starts, `free_irq` then waits out a handler still in flight, and the `TOP` enables
/// go last so a late handler cannot rearm them.
struct SelftestResources<'a, 'r> {
    _leaf_guard: LeafEnableGuard<'a>,
    reg: Pin<KBox<irq::Registration<'r, DoorbellTestHandler<'a>>>>,
    _top_guard: TopEnableGuard<'a>,
}

impl<'a> SelftestResources<'a, '_> {
    /// Returns the registered handler.
    fn handler(&self) -> &DoorbellTestHandler<'a> {
        self.reg.handler()
    }

    /// Disables the doorbell source and waits for a handler already running on another CPU.
    ///
    /// On return no further delivery can reach the handler, so its counters and the doorbell
    /// leaf hold their final values.
    fn quiesce_source(&self) {
        self.handler()
            .tree
            .disable_leaf(DOORBELL_VECTOR.leaf_index(), DOORBELL_VECTOR.leaf_mask());
        self.reg.synchronize();
    }
}

/// Runs the interrupt delivery self-test.
///
/// Quiesces the interrupt tree, registers a temporary handler, and injects the doorbell vector
/// twice, one delivery at a time. The handler, its IRQ registration, and all tree state are torn
/// down before this returns.
///
/// # Errors
///
/// `EINVAL` if the doorbell's subtree is not one nova-core services. `EIO` if the doorbell is
/// already pending before the test, if the delivery count is not two, if the doorbell bit is still
/// set once the source is stopped, or if either delivery found a pending bit other than the
/// doorbell. `ETIMEDOUT` if either delivery does not arrive within the timeout.
pub(crate) fn run_selftest<'a>(
    pdev: &'a pci::Device<Bound>,
    bar: Bar0<'a>,
    chipset: Chipset,
    vectors: &'a SubtreeVectors<'a>,
) -> Result {
    // The rearm method follows from the interrupt type, so take it from probe's allocation.
    let request = vectors.request_for(DOORBELL_SUBTREE)?;
    let msi_type = vectors.msi_type();
    let tree = Tree::new(bar, chipset, msi_type, DOORBELL_SUBTREE.into())?;
    let doorbell = DOORBELL_VECTOR.leaf_index();
    let doorbell_mask = DOORBELL_VECTOR.leaf_mask();

    dev_info!(
        pdev.as_ref(),
        "interrupt self-test: starting on vector {}, subtree {}, with {:?}\n",
        DOORBELL_VECTOR.into_raw(),
        DOORBELL_SUBTREE.index(),
        msi_type,
    );

    // No delivery may reach the CPU before a handler is registered, and a vector left enabled by
    // boot would fail the pending check below.
    tree.disable_all_leaves();
    tree.drain();

    // A delivery counts as the trigger's only if the vector starts out clear.
    let pre_pending = tree.read_pending(doorbell).vectors();
    if pre_pending.contains(doorbell_mask) {
        dev_warn!(
            pdev.as_ref(),
            "interrupt self-test: failed, vector {} already pending (leaf[{}] pending {:#x})\n",
            DOORBELL_VECTOR.into_raw(),
            doorbell.get(),
            pre_pending.into_raw(),
        );
        return Err(EIO);
    }

    let handler_init = try_pin_init!(DoorbellTestHandler {
        tree,
        first <- Completion::new(),
        second <- Completion::new(),
        irq_count: Atomic::new(0),
        first_pending: Atomic::new(0),
        second_pending: Atomic::new(0),
    }? Error);

    // Register the handler before any vector is enabled.
    let reg = KBox::pin_init(
        // SAFETY: the registration is owned by `resources` below and dropped before this function
        // returns, so its `Drop` (which calls `free_irq()`) always runs and the registration is
        // never leaked or `mem::forget`-ed.
        unsafe {
            irq::Registration::new(
                request,
                irq::Flags::TRIGGER_NONE,
                c"nova-core",
                handler_init,
            )
        },
        GFP_KERNEL,
    )?;

    // Initialized in the reverse of the declaration order that tears them down: the handler is
    // registered above, then the leaf enable, then the `TOP` enables.
    let resources = SelftestResources {
        _leaf_guard: reg
            .handler()
            .tree
            .enable_leaf_guarded(doorbell, doorbell_mask),
        _top_guard: reg.handler().tree.enable_top_guarded(),
        reg,
    };
    let handler = resources.handler();

    handler.tree.trigger(DOORBELL_VECTOR)?;
    let mut completed = handler
        .first
        .wait_for_completion_timeout(time::msecs_to_jiffies(DELIVERY_TIMEOUT_MS))
        .is_some();

    // Trigger the second interrupt only after the first handler has rearmed, so the two cannot
    // coalesce into one delivery.
    if completed {
        handler.tree.trigger(DOORBELL_VECTOR)?;
        completed = handler
            .second
            .wait_for_completion_timeout(time::msecs_to_jiffies(DELIVERY_TIMEOUT_MS))
            .is_some();
    }

    resources.quiesce_source();

    let count = handler.irq_count.load(Relaxed);
    let first_pending = LeafMask::from_raw(handler.first_pending.load(Relaxed));
    let second_pending = LeafMask::from_raw(handler.second_pending.load(Relaxed));
    let residual = handler.tree.read_pending(doorbell).vectors();

    // Nothing else can be pending in this leaf, so require the exact mask.
    if completed
        && count == 2
        && first_pending == doorbell_mask
        && second_pending == doorbell_mask
        && !residual.contains(doorbell_mask)
    {
        dev_info!(
            pdev.as_ref(),
            "interrupt self-test: passed, subtree {}, {} deliveries\n",
            DOORBELL_SUBTREE.index(),
            count,
        );
        Ok(())
    } else {
        dev_warn!(
            pdev.as_ref(),
            "interrupt self-test: failed, {} of 2 deliveries, leaf[{}] pending {:#x} and {:#x}, \
             {:#x} left set\n",
            count,
            doorbell.get(),
            first_pending.into_raw(),
            second_pending.into_raw(),
            residual.into_raw(),
        );
        Err(if completed { EIO } else { ETIMEDOUT })
    }
}
