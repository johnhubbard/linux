// SPDX-License-Identifier: GPL-2.0

use kernel::{
    io::{
        register::WithBase,
        Io, //
    },
    prelude::*, //
};

use crate::{
    driver::Bar0,
    falcon::{
        Falcon,
        FalconBromParams,
        FalconEngine, //
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

/// Returns whether `chipset`'s falcons implement `NV_PFALCON_FALCON_INTR_RETRIGGER`.
///
/// GA100 implements it and otherwise uses the Turing falcon HAL, so this is keyed on the
/// architecture rather than provided through [`FalconHal`].
pub(crate) fn has_intr_retrigger(chipset: Chipset) -> bool {
    !matches!(chipset.arch(), Architecture::Turing)
}

/// Returns whether `chipset` reaches `NV_PRISCV_RISCV_IRQMASK` and `NV_PRISCV_RISCV_IRQDEST` at
/// the Turing offsets rather than the GA102 ones.
fn has_turing_riscv_routing(chipset: Chipset) -> bool {
    matches!(chipset.arch(), Architecture::Turing) || chipset == Chipset::GA100
}

/// Returns the causes in `latched` that a RISC-V falcon on `chipset` routes to the host.
///
/// A cause reaches the host only if the core both enables it and directs it there. Every other
/// latched cause belongs to the firmware running on the core.
pub(crate) fn host_routed_causes<E: FalconEngine>(
    bar: Bar0<'_>,
    chipset: Chipset,
    latched: regs::NV_PFALCON_FALCON_IRQSTAT,
) -> regs::NV_PFALCON_FALCON_IRQSTAT {
    let routing = if has_turing_riscv_routing(chipset) {
        let mask = bar.read(regs::tu102::NV_PRISCV_RISCV_IRQMASK::of::<E>());
        let dest = bar.read(regs::tu102::NV_PRISCV_RISCV_IRQDEST::of::<E>());

        mask.value() & dest.value()
    } else {
        let mask = bar.read(regs::ga102::NV_PRISCV_RISCV_IRQMASK::of::<E>());
        let dest = bar.read(regs::ga102::NV_PRISCV_RISCV_IRQDEST::of::<E>());

        mask.value() & dest.value()
    };

    regs::NV_PFALCON_FALCON_IRQSTAT::from(latched.into_raw() & routing)
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
