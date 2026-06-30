// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use super::{
    CpuInterruptHal,
    LeafCount,
    MsiType,
    PciIrqRearmMethod, //
};

/// GIN parameters for Turing, Ampere, and Ada, which implement an 8-leaf CPU tree.
struct Tu102;

impl CpuInterruptHal for Tu102 {
    fn leaf_count(&self) -> LeafCount {
        LeafCount::Eight
    }

    fn pci_irq_rearm_method(&self, msi_type: MsiType) -> PciIrqRearmMethod {
        match msi_type {
            MsiType::Msi => PciIrqRearmMethod::ConfigMirrorEoi,
            MsiType::MsiX => PciIrqRearmMethod::TopEnableCycleSubtree,
        }
    }
}

const TU102: Tu102 = Tu102;
pub(super) const TU102_HAL: &dyn CpuInterruptHal = &TU102;
