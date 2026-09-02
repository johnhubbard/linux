// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! GPU interrupt support.
//!
//! GIN, the GPU Interrupt and Notification unit, is the GPU's interrupt controller: a two-level
//! tree of pending and enable registers, one tree per PCIe function.
//!
//! See `Documentation/gpu/nova/core/interrupts.rst`.

#[cfg(CONFIG_NOVA_CORE_IRQ_SELFTEST)]
pub(crate) mod doorbell_test;
pub(crate) mod gsp;
mod hal;
mod interrupt_tree;
mod regs;

use kernel::{
    device::Bound,
    irq,
    pci::{
        self,
        IrqType, //
    },
    prelude::*, //
};

use crate::{
    driver::Bar0,
    gpu::Chipset,
    num, //
};

use interrupt_tree::{
    Subtree,
    SubtreeSet,
    Tree, //
};

/// The message-signaled interrupt type a vector allocation obtained.
///
/// nova-core allocates MSI-X or MSI and nothing else, so the level-triggered INTx that
/// [`kernel::pci::IrqType`] also names has no representation here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MsiType {
    /// One message, raised by every subtree of the tree.
    Msi,

    /// One table entry per subtree.
    MsiX,
}

/// The PCI interrupt vector that delivers each serviced subtree.
///
/// MSI-X raises a separate table entry per subtree, so subtree `N` arrives on entry `N`. MSI has a
/// single message that every subtree raises, so all of them arrive on the one allocated entry.
pub(crate) struct SubtreeVectors<'a> {
    vectors: pci::IrqVectorRegistration<'a>,
    /// Every subtree nova-core services.
    serviced: SubtreeSet,
    /// The type [`alloc_vectors`] obtained, which fixes both the entry each subtree raises and the
    /// rearm write its handler owes.
    msi_type: MsiType,
}

impl SubtreeVectors<'_> {
    /// Returns the interrupt type these vectors were allocated as.
    pub(crate) fn msi_type(&self) -> MsiType {
        self.msi_type
    }

    /// Returns the interrupt tree these vectors deliver, as `chipset` implements it.
    ///
    /// # Errors
    ///
    /// `EINVAL` if this architecture does not implement a subtree these vectors service.
    fn tree<'b>(&self, bar: Bar0<'b>, chipset: Chipset) -> Result<Tree<'b>> {
        Tree::new(bar, chipset, self.msi_type, self.serviced)
    }

    /// Resets the interrupt tree these vectors deliver.
    ///
    /// Disables every vector in every implemented leaf, clears every pending bit, and rearms PCI
    /// interrupt delivery. On return no vector is enabled, so the tree delivers nothing.
    ///
    /// The rearm covers pre-Hopper MSI, where an interrupt delivered before probe leaves delivery
    /// un-armed with no handler to have rearmed it.
    ///
    /// Call this only during probe. It must not run concurrently with an interrupt handler.
    ///
    /// # Errors
    ///
    /// `EINVAL` if this architecture does not implement a subtree these vectors service.
    pub(crate) fn reset_tree(&self, bar: Bar0<'_>, chipset: Chipset) -> Result {
        let tree = self.tree(bar, chipset)?;

        tree.disable_all_leaves();
        tree.drain();
        for subtree in self.serviced.iter() {
            tree.rearm_pci_irq(subtree);
        }

        Ok(())
    }

    /// Returns an [`irq::IrqRequest`] for the vector that delivers `subtree`.
    ///
    /// MSI-X gives subtree `N` its own table entry `N`. MSI raises its one message from every
    /// subtree, and nova-core allocates a single entry for it.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `subtree` is not one nova-core services.
    pub(crate) fn request_for(&self, subtree: Subtree) -> Result<irq::IrqRequest<'_>> {
        if !self.serviced.contains(subtree) {
            return Err(EINVAL);
        }

        let entry = match self.msi_type {
            MsiType::MsiX => num::u32_as_usize(subtree.index()),
            MsiType::Msi => 0,
        };

        self.vectors.index(entry).map(Into::into)
    }
}

/// Allocates the interrupt vectors that the subtrees in `serviced` require.
///
/// Every subtree nova-core enables at `TOP` must have an allocated vector with a registered
/// handler, or the interrupts it raises are lost. Linux masks every MSI-X entry a driver did not
/// allocate, so the MSI-X request covers every entry up to the highest serviced subtree. A part
/// whose MSI-X table is smaller than that falls back to a single MSI, which serves the whole tree.
///
/// # Errors
///
/// `EINVAL` if `serviced` is empty. The error from the MSI request if neither type can be
/// allocated.
pub(crate) fn alloc_vectors(
    pdev: &pci::Device<Bound>,
    serviced: SubtreeSet,
) -> Result<SubtreeVectors<'_>> {
    if serviced.is_empty() {
        return Err(EINVAL);
    }

    // One entry per subtree up to and including the highest serviced one.
    let entries = serviced.span();

    let (vectors, msi_type) = pdev
        .alloc_irq_vectors(entries, entries, IrqType::MsiX.into())
        .map(|vectors| (vectors, MsiType::MsiX))
        .or_else(|_| {
            pdev.alloc_irq_vectors(1, 1, IrqType::Msi.into())
                .map(|vectors| (vectors, MsiType::Msi))
        })?;

    Ok(SubtreeVectors {
        vectors,
        serviced,
        msi_type,
    })
}
