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

use kernel::{
    device::Bound,
    pci::{
        self,
        IrqType,
        IrqTypes, //
    },
    prelude::*,
};

/// The PCI interrupt vector that delivers each serviced subtree.
///
/// MSI-X raises a separate table entry per subtree, so subtree `N` arrives on entry `N`. MSI has a
/// single message that every subtree raises, so all of them arrive on the one allocated entry.
#[derive(Clone, Copy)]
pub(crate) struct SubtreeVectors<'a> {
    vectors: pci::IrqAllocation<'a>,
    /// `TOP` bit of every subtree nova-core services.
    serviced: u32,
}

impl<'a> SubtreeVectors<'a> {
    /// Returns the interrupt type the PCI core selected for these vectors.
    pub(crate) fn irq_type(&self) -> IrqType {
        self.vectors.irq_type()
    }

    /// Returns the vector that delivers `subtree`, a single `TOP` bit of the form
    /// `interrupt_tree::vector_subtree_mask` returns.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `subtree` names anything other than a single subtree nova-core services.
    pub(crate) fn vector_for(&self, subtree: u32) -> Result<pci::IrqVector<'a>> {
        if subtree.count_ones() != 1 || subtree & self.serviced == 0 {
            return Err(EINVAL);
        }

        self.vectors.vector(entry_index(self.irq_type(), subtree))
    }
}

/// Returns the index of the allocated entry that `subtree` raises.
///
/// MSI-X gives subtree `N` its own table entry `N`. MSI raises its one message from every subtree,
/// and nova-core allocates a single entry for it. nova-core never allocates INTx.
fn entry_index(irq_type: IrqType, subtree: u32) -> u32 {
    match irq_type {
        IrqType::MsiX => subtree.trailing_zeros(),
        IrqType::Msi | IrqType::Intx => 0,
    }
}

/// Allocates the interrupt vectors that the subtrees in `serviced` require.
///
/// Every subtree nova-core enables at `TOP` must have an allocated vector with a registered
/// handler, or the interrupts it raises are lost. Linux masks every MSI-X entry a driver did not
/// allocate, so the MSI-X request covers every entry up to the highest serviced subtree. A part
/// whose MSI-X table is smaller than that falls back to a single MSI, which serves the whole tree.
/// nova-core does not fall back to a shared INTx line.
///
/// # Errors
///
/// `EINVAL` if `serviced` is empty. The error from the MSI request if neither type can be
/// allocated.
pub(crate) fn alloc_vectors(
    pdev: &pci::Device<Bound>,
    serviced: u32,
) -> Result<SubtreeVectors<'_>> {
    // One entry per subtree up to and including the highest serviced one.
    let msix_count = u32::BITS - serviced.leading_zeros();
    if msix_count == 0 {
        return Err(EINVAL);
    }

    let msix = IrqTypes::default().with(IrqType::MsiX);
    let vectors = match pdev.alloc_irq_vectors(msix_count, msix_count, msix) {
        Ok(vectors) => vectors,
        Err(_) => pdev.alloc_irq_vectors(1, 1, IrqTypes::default().with(IrqType::Msi))?,
    };

    Ok(SubtreeVectors { vectors, serviced })
}
