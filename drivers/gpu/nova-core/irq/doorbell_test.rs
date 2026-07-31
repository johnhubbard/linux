// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Interrupt delivery self-test.
//!
//! The test triggers the CPU doorbell vector from software, twice, and checks that each trigger
//! reaches a registered handler. It runs during probe under `CONFIG_NOVA_CORE_SELFTESTS`.
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

use super::interrupt_tree::{
    GinVector,
    LeafEnableGuard,
    LeafMask,
    Subtree,
    TopEnableGuard,
    Tree, //
};

use crate::{
    driver::Bar0,
    gpu::Chipset,
    selftest_assert,
    selftest_assert_eq, //
};

/// The CPU doorbell vector. Every supported GPU uses this number, so the test needs nothing from
/// GSP-RM, which is not running yet.
const DOORBELL_VECTOR: GinVector = GinVector::new::<129>();

/// The only subtree that this test services.
const DOORBELL_SUBTREE: Subtree = DOORBELL_VECTOR.subtree();

/// Time allowed for each delivery to arrive.
const DELIVERY_TIMEOUT_MS: time::Msecs = 1000;

/// The self-test's interrupt handler.
///
/// It clears only the doorbell's bit, rearms delivery, and never walks the tree. A missing rearm
/// shows up as a timeout on the second delivery.
#[pin_data]
struct DoorbellTestHandler<'a> {
    tree: Tree<'a>,
    /// Completed by the first delivery.
    #[pin]
    first: Completion,
    /// Completed by the second delivery.
    #[pin]
    second: Completion,
    /// Deliveries that found the doorbell bit set.
    irq_count: Atomic<u32>,
    /// The doorbell leaf's pending bits, as read by the first delivery.
    first_pending: Atomic<u32>,
    /// The doorbell leaf's pending bits, as read by the second delivery.
    second_pending: Atomic<u32>,
}

impl irq::Handler for DoorbellTestHandler<'_> {
    fn handle(&self) -> irq::IrqReturn {
        let leaf = self.tree.read_pending(DOORBELL_VECTOR.leaf_index());
        let pending = leaf.vectors();
        if !pending.contains(DOORBELL_VECTOR.leaf_mask()) {
            self.tree.rearm_pci_irq(DOORBELL_SUBTREE);
            return irq::IrqReturn::None;
        }
        leaf.clear_vectors(DOORBELL_VECTOR.leaf_mask());

        let count = self.irq_count.fetch_add(1, Relaxed);

        // Rearm before completing, since the waiting thread triggers the next doorbell as soon as
        // it wakes.
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

/// The self-test's handler registration and the enables that deliver to it.
///
/// Drops in the order that "Enabling the GSP event" in
/// `Documentation/gpu/nova/core/interrupts.rst` requires: the vector is disabled, then the
/// handler is freed, then the subtree is disabled.
struct SelftestResources<'a, 'r> {
    _leaf_guard: LeafEnableGuard<'a>,
    reg: Pin<KBox<irq::Registration<'r, DoorbellTestHandler<'a>>>>,
    _top_guard: TopEnableGuard<'a>,
}

impl<'a> SelftestResources<'a, '_> {
    fn handler(&self) -> &DoorbellTestHandler<'a> {
        self.reg.handler()
    }

    /// Disables the doorbell vector and waits for a handler in flight on another CPU to finish.
    ///
    /// The handler's counters and the leaf's pending bits are final on return.
    fn quiesce_source(&self) {
        self.handler()
            .tree
            .disable_leaf(DOORBELL_VECTOR.leaf_index(), DOORBELL_VECTOR.leaf_mask());
        self.reg.synchronize();
    }
}

/// Runs the interrupt delivery self-test.
///
/// Call this only during probe, before GSP boot: it disables every vector in the tree and clears
/// every pending bit. On return, the doorbell's subtree is disabled at `TOP`, and the test's PCI
/// vectors and handler are released.
///
/// # Errors
///
/// `EINVAL` if `chipset` does not implement the doorbell's subtree. `ETIMEDOUT` if a delivery
/// does not arrive within [`DELIVERY_TIMEOUT_MS`]. `EIO` if a self-test assertion fails.
/// Otherwise the error from allocating the PCI vectors or registering the handler.
pub(crate) fn run_selftest(pdev: &pci::Device<Bound>, bar: Bar0<'_>, chipset: Chipset) -> Result {
    let dev = pdev.as_ref();

    let vectors = super::alloc_vectors(pdev, DOORBELL_SUBTREE.into())?;
    let request = vectors.request_for(DOORBELL_SUBTREE)?;
    let tree = Tree::new(bar, chipset, &vectors)?;
    let doorbell = DOORBELL_VECTOR.leaf_index();
    let doorbell_mask = DOORBELL_VECTOR.leaf_mask();

    dev_info!(
        dev,
        "interrupt self-test: starting on vector {}, subtree {}, with {:?}\n",
        DOORBELL_VECTOR.into_raw(),
        DOORBELL_SUBTREE.index(),
        vectors.msi_type,
    );

    // GFW boot can leave vectors enabled and pending. Registering a handler unmasks the PCI
    // interrupt, and they would be delivered to a handler that services only the doorbell.
    tree.disable_all_leaves();
    tree.drain();

    // A delivery proves nothing unless the doorbell bit starts out clear.
    let pre_pending = tree.read_pending(doorbell).vectors();
    selftest_assert!(
        dev,
        !pre_pending.contains(doorbell_mask),
        "vector {} already pending, leaf[{}] is {:#x}",
        DOORBELL_VECTOR.into_raw(),
        doorbell.get(),
        pre_pending.into_raw()
    );

    let handler_init = try_pin_init!(DoorbellTestHandler {
        tree,
        first <- Completion::new(),
        second <- Completion::new(),
        irq_count: Atomic::new(0),
        first_pending: Atomic::new(0),
        second_pending: Atomic::new(0),
    }? Error);

    // Registration must precede any enable, or a delivery reaches no handler.
    let reg = KBox::pin_init(
        // SAFETY: this registration is dropped before the enclosing function returns, so its
        // `Drop`, which calls `free_irq()`, always runs.
        unsafe {
            irq::Registration::new(
                request,
                irq::Flags::TRIGGER_NONE,
                c"nova-core-selftest",
                handler_init,
            )
        },
        GFP_KERNEL,
    )?;

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

    // The second trigger waits for the first delivery, or the two could coalesce.
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

    if !completed {
        dev_err!(
            dev,
            "interrupt self-test: only {} of 2 deliveries arrived within {} ms\n",
            count,
            DELIVERY_TIMEOUT_MS,
        );
        return Err(ETIMEDOUT);
    }

    selftest_assert_eq!(dev, count, 2, "delivery count");

    // Every other vector in the leaf is disabled and was drained, so require the exact mask.
    selftest_assert_eq!(dev, first_pending, doorbell_mask, "first delivery");
    selftest_assert_eq!(dev, second_pending, doorbell_mask, "second delivery");
    selftest_assert!(
        dev,
        !residual.contains(doorbell_mask),
        "vector {} still pending, leaf[{}] is {:#x}",
        DOORBELL_VECTOR.into_raw(),
        doorbell.get(),
        residual.into_raw()
    );

    dev_info!(
        dev,
        "interrupt self-test: passed, subtree {}, {} deliveries\n",
        DOORBELL_SUBTREE.index(),
        count,
    );

    Ok(())
}
