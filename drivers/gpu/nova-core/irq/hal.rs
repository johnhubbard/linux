// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Per-architecture properties of the GIN CPU interrupt tree.

mod gh100;
mod tu102;

use kernel::{
    io::Io,
    pci::IrqType, //
};

use crate::{
    driver::Bar0,
    gpu::{
        Architecture,
        Chipset, //
    },
    regs, //
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
    /// `serviced` holds the `TOP` bit of every subtree the driver services, and `subtree` holds
    /// the bit of the one subtree the calling handler serves. Each method uses whichever of the
    /// two its interrupt type delivers on, so both are required.
    #[expect(dead_code)]
    pub(super) fn rearm(self, bar: Bar0<'_>, serviced: u32, subtree: u32) {
        let subtrees = match self {
            // The written value is ignored, so any write rearms delivery.
            Self::ConfigMirrorEoi => {
                bar.write(regs::tu102::NV_XVE_CYA_2, 0u32.into());
                return;
            }
            Self::TopEnableCycleServiced => serviced,
            Self::TopEnableCycleSubtree => subtree,
        };

        bar.write(
            regs::NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_CLEAR,
            subtrees.into(),
        );
        bar.write(
            regs::NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_SET,
            subtrees.into(),
        );
    }
}

/// Per-architecture properties of the GIN CPU interrupt tree.
///
/// The tree size and the method that rearms PCI interrupt delivery differ by family. The tree
/// walk, the vector encoding, and the read-and-clear sequence do not, and are in generic code.
///
/// See `Documentation/gpu/nova/core/interrupts.rst`.
pub(super) trait CpuInterruptHal {
    /// Returns the number of implemented interrupt leaves in the CPU tree.
    ///
    /// Each leaf is a 32-bit register, so the tree carries `num_leaves * 32` vectors.
    fn num_leaves(&self) -> usize;

    /// Returns the subtrees this architecture implements.
    ///
    /// Each `TOP` bit covers two adjacent leaves, so the tree has `num_leaves / 2` subtrees and
    /// the result has one bit set for each. Bits outside the result are not meaningful in
    /// `TOP_EN_SET` or `TOP_EN_CLEAR`.
    fn implemented_subtrees(&self) -> u32 {
        (1u32 << (self.num_leaves() / 2)) - 1
    }

    /// Returns the method that rearms PCI interrupt delivery for `irq_type`.
    ///
    /// `None` means that `irq_type` needs no rearm write. That is the case for `INTx`, which is
    /// level-triggered, and which nova-core does not allocate.
    #[expect(dead_code)]
    fn pci_irq_rearm_method(&self, irq_type: IrqType) -> Option<PciIrqRearmMethod>;
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
