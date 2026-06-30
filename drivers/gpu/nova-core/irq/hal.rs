// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Per-architecture properties of the GIN CPU interrupt tree.

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
    regs,
    MsiType, //
};

/// Register write that restores PCI interrupt delivery to the CPU.
///
/// A message-signaled interrupt is delivered once per edge, and the PCI side delivers no further
/// interrupt until the CPU rearms it. A handler that returns without this write receives no more
/// interrupts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PciIrqRearmMethod {
    /// The MSI end-of-interrupt register in the BAR0 PCI configuration-space mirror, used by
    /// MSI on pre-Hopper GPUs.
    ConfigMirrorEoi,

    /// A clear then a set of the `TOP` enable bits of every serviced subtree, which produces the
    /// edge that delivers the next interrupt.
    ///
    /// MSI has a single message that every subtree raises, so the rearm covers the whole serviced
    /// set.
    TopEnableCycleServiced,

    /// The same enable cycle, restricted to the one subtree the handler serves.
    ///
    /// MSI-X gives each subtree its own table entry and its own handler.
    TopEnableCycleSubtree,
}

impl PciIrqRearmMethod {
    /// Performs this method's register write.
    ///
    /// `serviced` holds every subtree the driver services, and `subtree` is the one subtree the
    /// calling handler serves. Each method uses whichever of the two its interrupt type delivers
    /// on, so both are required.
    pub(super) fn rearm(self, bar: Bar0<'_>, serviced: SubtreeSet, subtree: Subtree) {
        let subtrees = match self {
            // The written value is ignored, so any write rearms delivery.
            Self::ConfigMirrorEoi => {
                bar.write(regs::NV_XVE_CYA_2, 0u32.into());
                return;
            }
            Self::TopEnableCycleServiced => serviced,
            Self::TopEnableCycleSubtree => SubtreeSet::from(subtree),
        };

        bar.write_reg(
            regs::NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_CLEAR::zeroed().with_subtrees(subtrees),
        );
        bar.write_reg(
            regs::NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_SET::zeroed().with_subtrees(subtrees),
        );
    }
}

/// Per-architecture properties of the GIN CPU interrupt tree.
///
/// The tree size and the method that rearms PCI interrupt delivery differ by family.
///
/// See `Documentation/gpu/nova/core/interrupts.rst`.
pub(super) trait CpuInterruptHal {
    /// Returns the number of leaves the CPU tree implements.
    ///
    /// [`LeafCount::subtree_set`] gives the subtrees behind them, and
    /// [`LeafCount::vector_count`] the vectors they carry.
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
