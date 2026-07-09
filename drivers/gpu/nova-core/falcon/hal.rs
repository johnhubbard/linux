// SPDX-License-Identifier: GPL-2.0

use kernel::{
    io::{
        Io,
        Mmio, //
    },
    prelude::*, //
};

use crate::{
    falcon::{
        Falcon,
        FalconBromParams,
        FalconEngine,
        PFalcon2Registers, //
    },
    gpu::{
        Architecture,
        Chipset, //
    },
    regs,
};

mod ga102;
mod tu102;

/// Method used to load data into falcon memory. Some GPU architectures need
/// PIO and others can use DMA.
pub(crate) enum LoadMethod {
    /// Programmed I/O
    Pio,
    /// Direct Memory Access
    Dma,
}

/// Hardware Abstraction Layer for Falcon cores.
///
/// Implements chipset-specific low-level operations. The trait is generic against [`FalconEngine`]
/// so its `BASE` parameter can be used in order to avoid runtime bound checks when accessing
/// registers.
pub(crate) trait FalconHal<E: FalconEngine>: Send + Sync {
    /// Activates the Falcon core if the engine is a risvc/falcon dual engine.
    fn select_core(&self, _falcon: &Falcon<'_, E>) -> Result {
        Ok(())
    }

    /// Returns the fused version of the signature to use in order to run a HS firmware on this
    /// falcon instance. `engine_id_mask` and `ucode_id` are obtained from the firmware header.
    fn signature_reg_fuse_version(
        &self,
        falcon: &Falcon<'_, E>,
        engine_id_mask: u16,
        ucode_id: u8,
    ) -> Result<u32>;

    /// Program the boot ROM registers prior to starting a secure firmware.
    fn program_brom(&self, falcon: &Falcon<'_, E>, params: &FalconBromParams);

    /// Check if the RISC-V core is active.
    /// Returns `true` if the RISC-V core is active, `false` otherwise.
    fn is_riscv_active(&self, falcon: &Falcon<'_, E>) -> bool;

    /// Checks whether the RISC-V core is halted.
    ///
    /// Returns [`ENOTSUPP`] if the chipset does not expose RISC-V halt status.
    fn is_riscv_halted(&self, falcon: &Falcon<'_, E>) -> Result<bool>;

    /// Wait for memory scrubbing to complete.
    fn reset_wait_mem_scrubbing(&self, falcon: &Falcon<'_, E>) -> Result;

    /// Reset the falcon engine.
    fn reset_eng(&self, falcon: &Falcon<'_, E>) -> Result;

    /// Returns the method used to load data into the falcon's memory.
    ///
    /// The only chipsets supporting PIO are those < GA102, and PIO is the preferred method for
    /// these. For anything above, the PIO registers appear to be masked to the CPU, so DMA is the
    /// only usable method.
    fn load_method(&self) -> LoadMethod;
}

/// Offsets of a falcon's RISC-V interrupt routing registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RiscvRouting {
    /// The Turing offsets. GA100 uses them too.
    Tu102,

    /// The offsets from GA102 on.
    Ga102,
}

impl RiscvRouting {
    /// Returns the causes in `latched` that are routed to the host, meaning the CPU, rather than
    /// to the falcon's own RISC-V core.
    ///
    /// The causes routed to the core belong to the firmware running on it, and the host does not
    /// service them.
    pub(crate) fn host_routed_causes(
        self,
        pfalcon2: Mmio<'_, PFalcon2Registers>,
        latched: regs::NV_PFALCON_FALCON_IRQSTAT,
    ) -> regs::NV_PFALCON_FALCON_IRQSTAT {
        let (mask, dest) = match self {
            Self::Tu102 => (
                pfalcon2.read(regs::tu102::NV_PRISCV_RISCV_IRQMASK).value(),
                pfalcon2.read(regs::tu102::NV_PRISCV_RISCV_IRQDEST).value(),
            ),
            Self::Ga102 => (
                pfalcon2.read(regs::ga102::NV_PRISCV_RISCV_IRQMASK).value(),
                pfalcon2.read(regs::ga102::NV_PRISCV_RISCV_IRQDEST).value(),
            ),
        };

        regs::NV_PFALCON_FALCON_IRQSTAT::from(latched.into_raw() & mask & dest)
    }
}

/// Interrupt properties of a falcon that differ by GPU family.
///
/// Separate from [`FalconHal`] because the GSP event handler calls these from hard interrupt
/// context, where it cannot make the heap allocation that a `FalconHal` takes.
pub(crate) trait FalconIntrHal {
    /// Returns whether these falcons implement `NV_PFALCON_FALCON_INTR_RETRIGGER`.
    fn has_intr_retrigger(&self) -> bool;

    /// Returns the offsets of `PRISCV_RISCV_IRQMASK` and `PRISCV_RISCV_IRQDEST`.
    fn riscv_routing(&self) -> RiscvRouting;
}

/// Returns the [`FalconIntrHal`] for `chipset`.
///
/// GA100 has its own arm: it has the retrigger register, which Turing lacks, and the Turing
/// routing offsets, which GA102 moved.
pub(crate) fn falcon_intr_hal(chipset: Chipset) -> &'static dyn FalconIntrHal {
    match chipset.arch() {
        Architecture::Turing => tu102::TU102_INTR_HAL,
        Architecture::Ampere if chipset == Chipset::GA100 => tu102::GA100_INTR_HAL,
        Architecture::Ampere
        | Architecture::Ada
        | Architecture::Hopper
        | Architecture::BlackwellGB10x
        | Architecture::BlackwellGB20x => ga102::GA102_INTR_HAL,
    }
}

/// Returns a boxed falcon HAL adequate for `chipset`.
///
/// We use a heap-allocated trait object instead of a statically defined one because the
/// generic `FalconEngine` argument makes it difficult to define all the combinations
/// statically.
pub(super) fn falcon_hal<E: FalconEngine + 'static>(
    chipset: Chipset,
) -> Result<KBox<dyn FalconHal<E>>> {
    let hal = match chipset.arch() {
        Architecture::Turing => {
            KBox::new(tu102::Tu102::<E>::new(), GFP_KERNEL)? as KBox<dyn FalconHal<E>>
        }
        // GA100 boots like Turing so use Turing HAL
        Architecture::Ampere if chipset == Chipset::GA100 => {
            KBox::new(tu102::Tu102::<E>::new(), GFP_KERNEL)? as KBox<dyn FalconHal<E>>
        }
        Architecture::Ampere
        | Architecture::Ada
        | Architecture::Hopper
        | Architecture::BlackwellGB10x
        | Architecture::BlackwellGB20x => {
            KBox::new(ga102::Ga102::<E>::new(), GFP_KERNEL)? as KBox<dyn FalconHal<E>>
        }
    };

    Ok(hal)
}

#[kunit_tests(nova_core_falcon_hal)]
mod tests {
    use super::*;

    /// Turing falcons have no retrigger register. GA100 and every later chipset have it.
    #[test]
    fn intr_retrigger_gate_per_arch() {
        for chipset in [Chipset::TU102, Chipset::TU116] {
            assert!(!falcon_intr_hal(chipset).has_intr_retrigger());
        }

        for chipset in [
            Chipset::GA100,
            Chipset::GA102,
            Chipset::AD102,
            Chipset::GH100,
            Chipset::GB100,
            Chipset::GB202,
        ] {
            assert!(falcon_intr_hal(chipset).has_intr_retrigger());
        }
    }

    /// The RISC-V routing offsets change at GA102, so GA100 still uses the Turing ones.
    #[test]
    fn riscv_routing_offsets_split_at_ga102() {
        for chipset in [Chipset::TU102, Chipset::TU116, Chipset::GA100] {
            assert_eq!(
                falcon_intr_hal(chipset).riscv_routing(),
                RiscvRouting::Tu102
            );
        }

        for chipset in [
            Chipset::GA102,
            Chipset::AD102,
            Chipset::GH100,
            Chipset::GB100,
            Chipset::GB202,
        ] {
            assert_eq!(
                falcon_intr_hal(chipset).riscv_routing(),
                RiscvRouting::Ga102
            );
        }
    }
}
