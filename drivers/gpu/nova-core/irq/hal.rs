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

/// The register write that rearms PCI interrupt delivery after an interrupt.
///
/// The GPU family and the interrupt type that Linux granted select the write. See "Rearming PCI
/// interrupt delivery" in `Documentation/gpu/nova/core/interrupts.rst`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PciIrqRearmMethod {
    /// Writes the MSI end-of-interrupt register, `NV_XVE_CYA_2`. Pre-Hopper MSI.
    ConfigMirrorEoi,

    /// Clears and then sets the `TOP` enables of every serviced subtree. Hopper-plus MSI.
    TopEnableCycleServiced,

    /// Clears and then sets the `TOP` enable of the handler's own subtree. MSI-X.
    TopEnableCycleSubtree,
}

impl PciIrqRearmMethod {
    /// Rearms PCI interrupt delivery after a handler serviced `subtree`.
    ///
    /// `serviced` is every subtree that nova-core services, for the method that cycles them all.
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

/// The properties of the GIN CPU tree that differ by GPU family.
pub(super) trait CpuInterruptHal {
    /// Returns the number of leaves the tree implements.
    fn leaf_count(&self) -> LeafCount;

    /// Returns the rearm method for `msi_type`.
    fn pci_irq_rearm_method(&self, msi_type: MsiType) -> PciIrqRearmMethod;
}

/// Returns the [`CpuInterruptHal`] for `chipset`'s architecture.
pub(super) fn cpu_interrupt_hal(chipset: Chipset) -> &'static dyn CpuInterruptHal {
    match chipset.arch() {
        Architecture::Turing | Architecture::Ampere | Architecture::Ada => tu102::TU102_HAL,
        Architecture::Hopper | Architecture::BlackwellGB10x | Architecture::BlackwellGB20x => {
            gh100::GH100_HAL
        }
    }
}

#[kunit_tests(nova_core_gin_hal)]
mod tests {
    use super::*;

    use crate::gpu::Chipset;

    /// Turing through Ada implement an 8-leaf tree.
    #[test]
    fn pre_hopper_tree_size() {
        for chipset in [Chipset::TU102, Chipset::GA102, Chipset::AD102] {
            assert_eq!(cpu_interrupt_hal(chipset).leaf_count(), LeafCount::Eight);
        }
    }

    /// Hopper and later implement a 16-leaf tree.
    #[test]
    fn hopper_plus_tree_size() {
        for chipset in [Chipset::GH100, Chipset::GB100, Chipset::GB202] {
            assert_eq!(cpu_interrupt_hal(chipset).leaf_count(), LeafCount::Sixteen);
        }
    }

    /// MSI rearms through the configuration-space mirror only before Hopper. Hopper and later
    /// cycle the `TOP` enables of every serviced subtree.
    #[test]
    fn msi_rearm_method_per_arch() {
        for chipset in [Chipset::TU102, Chipset::GA102, Chipset::AD102] {
            let hal = cpu_interrupt_hal(chipset);
            assert_eq!(
                hal.pci_irq_rearm_method(MsiType::Msi),
                PciIrqRearmMethod::ConfigMirrorEoi
            );
        }

        for chipset in [Chipset::GH100, Chipset::GB100, Chipset::GB202] {
            let hal = cpu_interrupt_hal(chipset);
            assert_eq!(
                hal.pci_irq_rearm_method(MsiType::Msi),
                PciIrqRearmMethod::TopEnableCycleServiced
            );
        }
    }

    /// MSI-X rearms one subtree on every architecture, since each subtree has its own table
    /// entry.
    #[test]
    fn msix_rearms_one_subtree_on_every_arch() {
        for chipset in [
            Chipset::TU102,
            Chipset::GA102,
            Chipset::AD102,
            Chipset::GH100,
            Chipset::GB100,
            Chipset::GB202,
        ] {
            let hal = cpu_interrupt_hal(chipset);
            assert_eq!(
                hal.pci_irq_rearm_method(MsiType::MsiX),
                PciIrqRearmMethod::TopEnableCycleSubtree
            );
        }
    }
}
