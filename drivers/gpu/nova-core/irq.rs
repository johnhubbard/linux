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
mod regs;

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
