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
/// [`kernel::pci::IrqType`] also names has no variant here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MsiType {
    /// One message, which every subtree raises.
    Msi,

    /// One table entry per subtree.
    MsiX,
}

/// The PCI interrupt vectors allocated for the subtrees nova-core services.
///
/// Which entry a subtree arrives on follows from [`MsiType`]: MSI-X gives subtree `N` its own
/// table entry `N`, and MSI has one message that every subtree raises, so all of them arrive on
/// entry 0.
pub(crate) struct SubtreeVectors<'a> {
    vectors: pci::IrqVectorRegistration<'a>,
    /// Every subtree nova-core services.
    serviced: SubtreeSet,
    msi_type: MsiType,
}

impl SubtreeVectors<'_> {
    /// Returns the interrupt type these vectors were allocated as.
    #[cfg_attr(not(CONFIG_NOVA_CORE_IRQ_SELFTEST), expect(dead_code))]
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
    /// Clears every leaf enable and every pending bit, then rearms PCI interrupt delivery. No
    /// vector is enabled at its leaf on return, so the tree delivers nothing. Whether the
    /// serviced subtrees are left enabled at `TOP` depends on the rearm method.
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
/// Requests one MSI-X entry per subtree up to the highest one in `serviced`, and falls back to a
/// single MSI, which serves the whole tree. See "The serviced-subtree invariant" in
/// `Documentation/gpu/nova/core/interrupts.rst`.
///
/// # Errors
///
/// `EINVAL` if `serviced` is empty. Otherwise the error from the MSI request, when neither type
/// can be allocated.
pub(crate) fn alloc_vectors(
    pdev: &pci::Device<Bound>,
    serviced: SubtreeSet,
) -> Result<SubtreeVectors<'_>> {
    if serviced.is_empty() {
        return Err(EINVAL);
    }

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
