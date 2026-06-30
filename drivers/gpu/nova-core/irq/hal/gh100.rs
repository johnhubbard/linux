// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use kernel::pci::IrqType;

use super::{
    CpuInterruptHal,
    PciIrqRearmMethod, //
};

/// GIN parameters for Hopper and Blackwell, which implement a 16-leaf CPU tree. Only 12 leaves
/// carry sources.
struct Gh100;

impl CpuInterruptHal for Gh100 {
    fn num_leaves(&self) -> usize {
        16
    }

    fn pci_irq_rearm_method(&self, irq_type: IrqType) -> Option<PciIrqRearmMethod> {
        match irq_type {
            IrqType::Intx => None,
            IrqType::Msi => Some(PciIrqRearmMethod::TopEnableCycleServiced),
            IrqType::MsiX => Some(PciIrqRearmMethod::TopEnableCycleSubtree),
        }
    }
}

const GH100: Gh100 = Gh100;
pub(super) const GH100_HAL: &dyn CpuInterruptHal = &GH100;
