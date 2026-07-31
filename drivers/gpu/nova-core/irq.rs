// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! GPU interrupt support.
//!
//! GIN, the GPU Interrupt and Notification unit, is the GPU's interrupt controller. It latches
//! every interrupt source in a two-level register tree and delivers the tree to the CPU as a
//! message-signaled PCI interrupt.
//!
//! See `Documentation/gpu/nova/core/interrupts.rst`.

#[cfg(CONFIG_NOVA_CORE_SELFTESTS)]
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
    Subtree,
    SubtreeSet, //
};

/// The message-signaled interrupt type that Linux granted.
///
/// nova-core never requests INTx, so this has no variant for it, unlike [`kernel::pci::IrqType`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MsiType {
    /// A single message, which every subtree raises.
    Msi,

    /// One table entry per subtree.
    MsiX,
}

/// The PCI interrupt vectors allocated for the subtrees that nova-core services.
///
/// A subtree may be enabled at `TOP` only once a vector is allocated for it and a handler is
/// registered on that vector. See "The serviced-subtree invariant" in
/// `Documentation/gpu/nova/core/interrupts.rst`.
pub(crate) struct SubtreeVectors<'a> {
    vectors: pci::IrqVectorRegistration<'a>,
    serviced: SubtreeSet,
    msi_type: MsiType,
}

impl SubtreeVectors<'_> {
    /// Returns the [`irq::IrqRequest`] for the PCI vector that delivers `subtree`.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `subtree` is not one of the serviced subtrees.
    fn request_for(&self, subtree: Subtree) -> Result<irq::IrqRequest<'_>> {
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

/// Allocates the PCI interrupt vectors for the subtrees in `serviced`.
///
/// Requests MSI-X entries `0` through the highest subtree in `serviced`, since an allocation
/// cannot be sparse, and falls back to a single MSI message for the whole tree.
///
/// # Errors
///
/// `EINVAL` if `serviced` is empty. Otherwise, when neither type could be allocated, the error
/// from the MSI request.
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
