// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use kernel::pci::IrqType;

use super::{
    CpuInterruptHal,
    PciIrqRearmMethod, //
};

/// GIN parameters for Turing, Ampere, and Ada, which implement an 8-leaf CPU tree.
struct Tu102;

impl CpuInterruptHal for Tu102 {
    fn num_leaves(&self) -> usize {
        8
    }

    fn pci_irq_rearm_method(&self, irq_type: IrqType) -> Option<PciIrqRearmMethod> {
        match irq_type {
            IrqType::Intx => None,
            IrqType::Msi => Some(PciIrqRearmMethod::ConfigMirrorEoi),
            IrqType::MsiX => Some(PciIrqRearmMethod::TopEnableCycleSubtree),
        }
    }
}

const TU102: Tu102 = Tu102;
pub(super) const TU102_HAL: &dyn CpuInterruptHal = &TU102;
