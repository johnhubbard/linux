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

use interrupt_tree::{
    Subtree,
    SubtreeSet, //
};

/// The PCI interrupt vector that delivers each serviced subtree.
///
/// MSI-X raises a separate table entry per subtree, so subtree `N` arrives on entry `N`. MSI has a
/// single message that every subtree raises, so all of them arrive on the one allocated entry.
pub(crate) struct SubtreeVectors<'a> {
    vectors: pci::IrqVectorRegistration<'a>,
    /// Every subtree nova-core services.
    serviced: SubtreeSet,
}

impl SubtreeVectors<'_> {
    /// Returns the interrupt type the PCI core selected for these vectors.
    pub(crate) fn irq_type(&self) -> IrqType {
        self.vectors.irq_type()
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

        self.vectors
            .index(entry_index(self.irq_type(), subtree))
            .map(Into::into)
    }
}

/// Returns the index of the allocated entry that `subtree` raises.
///
/// MSI-X gives subtree `N` its own table entry `N`. MSI raises its one message from every subtree,
/// and nova-core allocates a single entry for it. nova-core never allocates INTx.
fn entry_index(irq_type: IrqType, subtree: Subtree) -> usize {
    match irq_type {
        IrqType::MsiX => crate::num::u32_as_usize(subtree.index()),
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
    serviced: SubtreeSet,
) -> Result<SubtreeVectors<'_>> {
    if serviced.is_empty() {
        return Err(EINVAL);
    }

    // One entry per subtree up to and including the highest serviced one.
    let entries = serviced.span();

    let vectors = match pdev.alloc_irq_vectors(entries, entries, IrqType::MsiX.into()) {
        Ok(vectors) => vectors,
        Err(_) => pdev.alloc_irq_vectors(1, 1, IrqType::Msi.into())?,
    };

    Ok(SubtreeVectors { vectors, serviced })
}
