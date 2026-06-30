// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! GPU interrupt support.
//!
//! GIN, the GPU Interrupt and Notification unit, is the GPU's interrupt controller: a two-level
//! tree of pending and enable registers, one tree per PCIe function.
//!
//! See `Documentation/gpu/nova/core/interrupts.rst`.

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

pub(crate) fn alloc_vector(pdev: &pci::Device<Bound>) -> Result<pci::IrqVector<'_>> {
    let msi_types = IrqTypes::default().with(IrqType::Msi).with(IrqType::MsiX);

    let irq_vectors = match pdev.alloc_irq_vectors(1, 1, msi_types) {
        Ok(vecs) => vecs,
        Err(_) => {
            dev_warn!(pdev.as_ref(), "MSI not available, falling back to INTx\n");
            pdev.alloc_irq_vectors(1, 1, IrqTypes::default().with(IrqType::Intx))?
        }
    };

    irq_vectors.vector(0)
}
