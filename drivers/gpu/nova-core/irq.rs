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

use crate::num;

use interrupt_tree::{
    GinVector,
    Subtree,
    SubtreeSet, //
};

/// The subtree nova-core allocates PCI vectors for.
///
/// Every source nova-core services latches in this one subtree, so a single allocation covers all
/// of them.
pub(crate) const SERVICED_SUBTREE: Subtree = GinVector::new::<129>().subtree();

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
    pub(crate) fn msi_type(&self) -> MsiType {
        self.msi_type
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
