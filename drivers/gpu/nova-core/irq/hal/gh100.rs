// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use super::{
    CpuInterruptHal,
    LeafCount,
    MsiType,
    PciIrqRearmMethod, //
};

/// The CPU interrupt tree properties of Hopper and Blackwell.
struct Gh100;

impl CpuInterruptHal for Gh100 {
    fn leaf_count(&self) -> LeafCount {
        LeafCount::Sixteen
    }

    fn pci_irq_rearm_method(&self, msi_type: MsiType) -> PciIrqRearmMethod {
        match msi_type {
            MsiType::Msi => PciIrqRearmMethod::TopEnableCycleServiced,
            MsiType::MsiX => PciIrqRearmMethod::TopEnableCycleSubtree,
        }
    }
}

const GH100: Gh100 = Gh100;
pub(super) const GH100_HAL: &dyn CpuInterruptHal = &GH100;
