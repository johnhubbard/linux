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
/// Turing falcons do not. Ampere and later do, including GA100, whose falcon otherwise uses the
/// Turing HAL, so this is keyed on the architecture rather than provided through [`FalconHal`].
pub(crate) fn has_intr_retrigger(chipset: Chipset) -> bool {
    !matches!(chipset.arch(), Architecture::Turing)
}

/// Returns whether `chipset` carries the RISC-V interrupt routing registers at the Turing
/// offsets.
///
/// GA102 moved `NV_PRISCV_RISCV_IRQMASK` and `NV_PRISCV_RISCV_IRQDEST`, and GA100 kept the Turing
/// offsets, which is also why [`falcon_hal`] gives GA100 the Turing HAL.
fn has_turing_riscv_routing(chipset: Chipset) -> bool {
    matches!(chipset.arch(), Architecture::Turing) || chipset == Chipset::GA100
}

/// Returns the interrupt causes a RISC-V falcon on `chipset` routes to the host, in the layout of
/// `NV_PFALCON_FALCON_IRQSTAT`.
///
/// A cause reaches the host only if the RISC-V core both enables it and directs it there, which
/// `NV_PRISCV_RISCV_IRQMASK` and `NV_PRISCV_RISCV_IRQDEST` say. Every other latched cause belongs
/// to the firmware running on the core.
pub(crate) fn host_intr_routing<E: FalconEngine>(bar: Bar0<'_>, chipset: Chipset) -> u32 {
    if has_turing_riscv_routing(chipset) {
        bar.read(regs::tu102::NV_PRISCV_RISCV_IRQMASK::of::<E>())
            .value()
            & bar
                .read(regs::tu102::NV_PRISCV_RISCV_IRQDEST::of::<E>())
                .value()
    } else {
        bar.read(regs::ga102::NV_PRISCV_RISCV_IRQMASK::of::<E>())
            .value()
            & bar
                .read(regs::ga102::NV_PRISCV_RISCV_IRQDEST::of::<E>())
                .value()
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
