// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Interrupt delivery self-test, driven through the CPU doorbell vector.
//!
//! Exercises the whole PCI interrupt path (GPU to PCIe to CPU to handler) with no GSP dependency:
//! it injects a known vector through the GIN software trigger and confirms the handler runs. Two
//! interrupts are triggered one at a time, which also covers the rearm that every delivery after
//! the first depends on. Gated behind `CONFIG_NOVA_CORE_IRQ_SELFTEST` and run before GSP boot, so
//! it never observes or clears GSP interrupt state.
//!
//! See `Documentation/gpu/nova/core/interrupts.rst`.

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
        vector_leaf_bit,
        vector_subtree_mask,
        LeafIndex,
        Tree, //
    },
    SubtreeVectors, //
};
use crate::{
    driver::Bar0,
    gpu::Chipset, //
};

/// Fixed vector for the CPU doorbell.
///
/// The resource manager pins the CPU doorbell to this vector on every supported chip, so nova-core
/// uses the constant directly instead of discovering it at runtime.
const DOORBELL_VECTOR: u32 = 129;

/// Leaf and bit index of the doorbell vector within the interrupt tree.
const DOORBELL_LOC: (usize, u32) = vector_leaf_bit(DOORBELL_VECTOR);

/// Leaf holding the doorbell vector.
const DOORBELL_LEAF: usize = DOORBELL_LOC.0;

/// Bit of the doorbell vector within its leaf.
const DOORBELL_BIT: u32 = 1 << DOORBELL_LOC.1;

/// Subtree carrying the doorbell vector, and the only subtree this test services.
///
/// Derived from the vector so that changing `DOORBELL_VECTOR` moves the subtree it enables and the
/// handler together.
const DOORBELL_SUBTREE: u32 = vector_subtree_mask(DOORBELL_VECTOR);

/// Index of the subtree carrying the doorbell vector. Under MSI-X this is also the index of the
/// table entry that subtree raises.
const DOORBELL_SUBTREE_INDEX: u32 = DOORBELL_SUBTREE.trailing_zeros();

/// Time allowed for each of the two deliveries to arrive.
const DELIVERY_TIMEOUT_MS: time::Msecs = 1000;

/// Interrupt handler installed by the self-test.
///
/// Services the doorbell the way a notification source is serviced: it clears its own leaf bit and
/// rearms PCI interrupt delivery, leaving the rest of the tree untouched. It records the leaf's
/// pending bits seen on each of the first two deliveries and signals the matching completion.
#[pin_data]
struct DoorbellTestHandler<'a> {
    /// Borrowed BAR0, for register access from interrupt context.
    bar: Bar0<'a>,
    tree: Tree,
    /// Signalled by the first delivery.
    #[pin]
    first: Completion,
    /// Signalled by the second delivery.
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
        let bar = self.bar;

        // Clear only this handler's own bit and leave `TOP_EN` alone. A full walk disables and
        // enables the tree, which produces a delivery edge by itself and would hide a missing PCI
        // interrupt rearm.
        let leaf = self
            .tree
            .leaf(LeafIndex::new::<DOORBELL_LEAF>())
            .read_pending(bar);
        let pending = leaf.pending_bits();
        if pending & DOORBELL_BIT == 0 {
            self.tree.rearm_pci_irq(bar, DOORBELL_SUBTREE);
            return irq::IrqReturn::None;
        }
        leaf.clear_vectors(bar, DOORBELL_BIT);

        let count = self.irq_count.fetch_add(1, Relaxed);

        // Rearm before signalling, so delivery is possible again by the time the waiting thread
        // triggers the next vector.
        self.tree.rearm_pci_irq(bar, DOORBELL_SUBTREE);

        match count {
            0 => {
                self.first_pending.store(pending, Relaxed);
                self.first.complete_all();
            }
            1 => {
                self.second_pending.store(pending, Relaxed);
                self.second.complete_all();
            }
            _ => (),
        }

        irq::IrqReturn::Handled
    }
}

/// Teardown guard for the self-test.
///
/// Owns the IRQ registration so that every exit path, including an early error, tears down the
/// interrupt in this order: disabling the leaf stops new deliveries, dropping the registration
/// runs `free_irq()`, which waits for a handler still in flight, and only then are the tree's
/// subtrees disabled, so a late handler cannot rearm them.
struct SelftestGuard<'a, 'r> {
    bar: Bar0<'a>,
    tree: Tree,
    doorbell: LeafIndex,
    reg: Option<Pin<KBox<irq::Registration<'r, DoorbellTestHandler<'a>>>>>,
}

impl<'a, 'r> SelftestGuard<'a, 'r> {
    /// Returns the registered handler.
    fn handler(&self) -> &DoorbellTestHandler<'a> {
        // `reg` is `Some` for the whole lifetime of the guard. Only `drop` clears it.
        self.reg.as_ref().unwrap().handler()
    }

    /// Disables the doorbell source and waits for a handler already running on another CPU.
    ///
    /// On return no further delivery can reach the handler, so its counters and the doorbell
    /// leaf hold their final values.
    fn quiesce_source(&self) {
        self.tree
            .leaf(self.doorbell)
            .disable(self.bar, DOORBELL_BIT);
        // `reg` is `Some` for the whole lifetime of the guard. Only `drop` clears it.
        self.reg.as_ref().unwrap().synchronize();
    }
}

impl Drop for SelftestGuard<'_, '_> {
    fn drop(&mut self) {
        self.tree
            .leaf(self.doorbell)
            .disable(self.bar, DOORBELL_BIT);
        self.reg = None;
        self.tree.top().disable(self.bar);
    }
}

/// Runs the interrupt delivery self-test.
///
/// Quiesces the interrupt tree, registers a temporary handler, and injects the doorbell vector
/// through the GIN software trigger twice, one delivery at a time. This validates the PCI
/// interrupt path from GIN to the ISR without GSP firmware, including the rearm without which only
/// the first interrupt would arrive. The handler, its IRQ registration, and all tree state are
/// torn down before this returns.
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
    vectors: SubtreeVectors<'_>,
) -> Result {
    // The interrupt type decides how the handler rearms delivery, so the tree takes it from
    // probe's allocation.
    let vector = vectors.vector_for(DOORBELL_SUBTREE)?;
    let irq_type = vectors.irq_type();
    let tree = Tree::new(chipset, irq_type, DOORBELL_SUBTREE);
    let doorbell = LeafIndex::new::<DOORBELL_LEAF>();

    // Under MSI-X the subtree index is also the table entry the delivery arrives on, so a pass
    // shows that the per-subtree routing works. Under MSI every subtree shares one entry.
    dev_info!(
        pdev.as_ref(),
        "interrupt self-test: starting on vector {}, subtree {}, with {:?}\n",
        DOORBELL_VECTOR,
        DOORBELL_SUBTREE_INDEX,
        irq_type,
    );

    // No delivery may reach the CPU before a handler is registered. `drain` enables the top level
    // as the last step of its cycle, so disable it again afterward.
    tree.leaf(doorbell).disable(bar, DOORBELL_BIT);
    tree.drain(bar);
    tree.top().disable(bar);

    // A delivery can be credited to the trigger below only if the vector starts out clear, so
    // refuse to run otherwise.
    let pre_pending = tree.leaf(doorbell).read_pending(bar).pending_bits();
    if pre_pending & DOORBELL_BIT != 0 {
        dev_warn!(
            pdev.as_ref(),
            "interrupt self-test: failed, vector {} already pending (leaf[{}] pending {:#x})\n",
            DOORBELL_VECTOR,
            DOORBELL_LEAF,
            pre_pending,
        );
        return Err(EIO);
    }

    // `try_pin_init!` moves its captures, so the handler takes a clone and `tree` stays available
    // for the guard below.
    let handler_tree = tree.clone();
    let handler_init = try_pin_init!(DoorbellTestHandler {
        bar,
        tree: handler_tree,
        first <- Completion::new(),
        second <- Completion::new(),
        irq_count: Atomic::new(0),
        first_pending: Atomic::new(0),
        second_pending: Atomic::new(0),
    }? Error);

    // Register the handler before allowing any source to fire.
    let reg = KBox::pin_init(
        // SAFETY: the registration is owned by `guard` below and dropped before this function
        // returns, so its `Drop` (which calls `free_irq()`) always runs and the registration is
        // never leaked or `mem::forget`-ed.
        unsafe { pdev.request_irq(vector, irq::Flags::TRIGGER_NONE, c"nova-core", handler_init) },
        GFP_KERNEL,
    )?;

    // From here every exit must tear down the source, the registration, and the tree, so hand the
    // registration to a guard that does so on drop.
    let guard = SelftestGuard {
        bar,
        tree: tree.clone(),
        doorbell,
        reg: Some(reg),
    };
    let handler = guard.handler();

    // The handler is registered, so the source can be enabled.
    handler.tree.leaf(doorbell).enable(bar, DOORBELL_BIT);
    handler.tree.top().enable(bar);

    handler.tree.trigger(bar, DOORBELL_VECTOR)?;
    let mut completed = handler
        .first
        .wait_for_completion_timeout(time::msecs_to_jiffies(DELIVERY_TIMEOUT_MS))
        .is_some();

    // Trigger the second interrupt only once the first handler has cleared its leaf bit and
    // rearmed, so the two cannot coalesce into one delivery and a handler that never rearms
    // cannot pass.
    if completed {
        handler.tree.trigger(bar, DOORBELL_VECTOR)?;
        completed = handler
            .second
            .wait_for_completion_timeout(time::msecs_to_jiffies(DELIVERY_TIMEOUT_MS))
            .is_some();
    }

    // Stop the source and wait out any handler still running, so the values read below are the
    // final ones.
    guard.quiesce_source();

    let count = handler.irq_count.load(Relaxed);
    let first_pending = handler.first_pending.load(Relaxed);
    let second_pending = handler.second_pending.load(Relaxed);
    let residual = tree.leaf(doorbell).read_pending(bar).pending_bits();

    // The self-test runs before GSP boot on a leaf that `drain` has just cleared, and nothing
    // triggers the vector after the second delivery, so each delivery must find the doorbell bit
    // and nothing else, and the leaf must end clear.
    if completed
        && count == 2
        && first_pending == DOORBELL_BIT
        && second_pending == DOORBELL_BIT
        && residual & DOORBELL_BIT == 0
    {
        dev_info!(
            pdev.as_ref(),
            "interrupt self-test: passed, subtree {}, {} deliveries\n",
            DOORBELL_SUBTREE_INDEX,
            count,
        );
        Ok(())
    } else {
        dev_warn!(
            pdev.as_ref(),
            "interrupt self-test: failed, {} of 2 deliveries, leaf[{}] pending {:#x} and {:#x}, \
             {:#x} left set\n",
            count,
            DOORBELL_LEAF,
            first_pending,
            second_pending,
            residual,
        );
        Err(if completed { EIO } else { ETIMEDOUT })
    }
}
