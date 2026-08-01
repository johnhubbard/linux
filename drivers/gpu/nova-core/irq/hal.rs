// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Per-architecture properties of the GIN CPU interrupt tree.
//!
//! See "Per-architecture differences" in `Documentation/gpu/nova/core/interrupts.rst`.

mod gh100;
mod tu102;

use kernel::{
    io::Io,
    prelude::*, //
};

use crate::{
    driver::Bar0,
    gpu::{
        Architecture,
        Chipset, //
    }, //
};

use super::{
    interrupt_tree::{
        LeafCount,
        Subtree,
        SubtreeSet, //
    },
    regs::*,
    MsiType, //
};

/// Register write that restores PCI interrupt delivery to the CPU.
///
/// A handler performs it once per interrupt it services, or it receives no further interrupts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PciIrqRearmMethod {
    /// The MSI end-of-interrupt register in the BAR0 PCI configuration-space mirror. Pre-Hopper
    /// MSI only.
    ConfigMirrorEoi,

    /// A clear then a set of the `TOP` enable bits of every serviced subtree, which produces the
    /// edge that delivers the next interrupt.
    TopEnableCycleServiced,

    /// The same enable cycle, restricted to the one subtree the handler serves.
    TopEnableCycleSubtree,
}

impl PciIrqRearmMethod {
    /// Performs this method's register write, for a handler that has just serviced `subtree`.
    ///
    /// `serviced` is every subtree the driver services, which the MSI methods cycle as a whole.
    pub(super) fn rearm(self, bar: Bar0<'_>, serviced: SubtreeSet, subtree: Subtree) {
        let subtrees = match self {
            Self::ConfigMirrorEoi => {
                bar.write(NV_XVE_CYA_2, 0u32.into());
                return;
            }
            Self::TopEnableCycleServiced => serviced,
            Self::TopEnableCycleSubtree => SubtreeSet::from(subtree),
        };

        bar.write_reg(
            NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_CLEAR::zeroed().with_subtrees(subtrees),
        );
        bar.write_reg(
            NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_SET::zeroed().with_subtrees(subtrees),
        );
    }
}

/// The two GIN tree properties that differ by GPU family.
pub(super) trait CpuInterruptHal {
    /// Returns the number of leaves the CPU tree implements.
    fn leaf_count(&self) -> LeafCount;

    /// Returns the method that rearms PCI interrupt delivery for `msi_type`.
    fn pci_irq_rearm_method(&self, msi_type: MsiType) -> PciIrqRearmMethod;
}

/// Returns the [`CpuInterruptHal`] for `chipset`.
pub(super) fn cpu_interrupt_hal(chipset: Chipset) -> &'static dyn CpuInterruptHal {
    match chipset.arch() {
        Architecture::Turing | Architecture::Ampere | Architecture::Ada => tu102::TU102_HAL,
        Architecture::Hopper | Architecture::BlackwellGB10x | Architecture::BlackwellGB20x => {
            gh100::GH100_HAL
        }
    }
}
