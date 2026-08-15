// SPDX-License-Identifier: GPL-2.0

//! Bootloader support for the FWSEC firmware.
//!
//! On Turing, the FWSEC firmware is not loaded directly, but is instead loaded through a small
//! bootloader program that performs the required DMA operations. This bootloader itself needs to
//! be loaded using PIO.

use kernel::{
    device::{
        self,
        Device, //
    },
    dma::Coherent,
    io::{register::WithBase, Io},
    prelude::*,
    transmute::AsBytes,
};

use crate::{
    driver::Bar0,
    falcon::{
        gsp::Gsp,
        Falcon,
        FalconBromParams,
        FalconDmaLoadable,
        FalconFbifMemType,
        FalconFbifTarget,
        FalconFirmware,
        FalconPioDmemLoadTarget,
        FalconPioImemLoadTarget,
        FalconPioLoadable, //
    },
    firmware::{
        fwsec::FwsecFirmware,
        gen_bootloader::{
            BootloaderDmemDescV2,
            GenericBootloader, //
        },
    },
    gpu::Chipset,
    num::FromSafeCast, //
    regs,
};

/// Wrapper for [`FwsecFirmware`] that includes the bootloader performing the actual load
/// operation.
pub(crate) struct FwsecFirmwareWithBl {
    /// DMA object the bootloader will copy the firmware from.
    _firmware_dma: Coherent<[u8]>,
    /// Bootloader that performs the load.
    bootloader: GenericBootloader,
    /// Descriptor to be loaded into DMEM for the bootloader to read.
    dmem_desc: BootloaderDmemDescV2,
    /// BROM parameters of the loaded firmware.
    brom_params: FalconBromParams,
}

impl FwsecFirmwareWithBl {
    /// Loads the bootloader firmware for `dev` and `chipset`, and wrap `firmware` so it can be
    /// loaded using it.
    pub(crate) fn new(
        firmware: FwsecFirmware,
        dev: &Device<device::Bound>,
        chipset: Chipset,
        falcon: &Falcon<'_, Gsp>,
    ) -> Result<Self> {
        let bootloader = GenericBootloader::new(dev, chipset, falcon)?;

        // `BootloaderDmemDescV2` expects the source to be a mirror image of the destination and
        // uses the same offset parameter for both.
        //
        // Thus, the start of the source object needs to be padded with the difference between the
        // destination and source offsets.
        //
        // In practice, this is expected to always be zero but is required for code correctness.
        let (align_padding, firmware_dma) = {
            let align_padding = {
                let imem_sec = firmware.imem_sec_load_params();

                imem_sec
                    .dst_start
                    .checked_sub(imem_sec.src_start)
                    .map(usize::from_safe_cast)
                    .ok_or(EOVERFLOW)?
            };

            let mut firmware_obj = KVVec::new();
            firmware_obj.extend_with(align_padding, 0u8, GFP_KERNEL)?;
            firmware_obj.extend_from_slice(firmware.ucode.0.as_slice(), GFP_KERNEL)?;

            (
                align_padding,
                Coherent::from_slice(dev, firmware_obj.as_slice(), GFP_KERNEL)?,
            )
        };

        let dmem_desc = {
            // Bootloader payload is in non-coherent system memory.
            const FALCON_DMAIDX_PHYS_SYS_NCOH: u32 = 4;

            let imem_sec = firmware.imem_sec_load_params();
            let imem_ns = firmware.imem_ns_load_params().ok_or(EINVAL)?;
            let dmem = firmware.dmem_load_params();

            // The bootloader does not have a data destination offset field and copies the data at
            // the start of DMEM, so it can only be used if the destination offset of the firmware
            // is 0.
            if dmem.dst_start != 0 {
                return Err(EINVAL);
            }

            BootloaderDmemDescV2 {
                reserved: [0; 4],
                signature: [0; 4],
                ctx_dma: FALCON_DMAIDX_PHYS_SYS_NCOH,
                code_dma_base: firmware_dma.dma_address(),
                // `dst_start` is also valid as the source offset since the firmware DMA object is
                // a mirror image of the target IMEM layout.
                non_sec_code_off: imem_ns.dst_start,
                non_sec_code_size: imem_ns.len,
                // `dst_start` is also valid as the source offset since the firmware DMA object is
                // a mirror image of the target IMEM layout.
                sec_code_off: imem_sec.dst_start,
                sec_code_size: imem_sec.len,
                code_entry_point: 0,
                // Start of data section is the added padding + the DMEM `src_start` field.
                data_dma_base: firmware_dma
                    .dma_address()
                    .checked_add(u64::from_safe_cast(align_padding))
                    .and_then(|offset| offset.checked_add(dmem.src_start.into()))
                    .ok_or(EOVERFLOW)?,
                data_size: dmem.len,
                argc: 0,
                argv: 0,
            }
        };

        Ok(Self {
            _firmware_dma: firmware_dma,
            bootloader,
            dmem_desc,
            brom_params: firmware.brom_params(),
        })
    }

    /// Loads the bootloader into `falcon` and execute it.
    ///
    /// The bootloader will load the FWSEC firmware and then execute it. This function returns
    /// after FWSEC has reached completion.
    pub(crate) fn run(
        &self,
        dev: &Device<device::Bound>,
        falcon: &Falcon<'_, Gsp>,
        bar: Bar0<'_>,
    ) -> Result<()> {
        // Reset falcon, load the firmware, and run it.
        falcon
            .reset()
            .inspect_err(|e| dev_err!(dev, "Failed to reset GSP falcon: {:?}\n", e))?;
        falcon
            .pio_load(self)
            .inspect_err(|e| dev_err!(dev, "Failed to load FWSEC firmware: {:?}\n", e))?;

        // Configure DMA index for the bootloader to fetch the FWSEC firmware from system memory.
        bar.update(
            regs::NV_PFALCON_FBIF_TRANSCFG::of::<Gsp>()
                .try_at(usize::from_safe_cast(self.dmem_desc.ctx_dma))
                .ok_or(EINVAL)?,
            |v| {
                v.with_target(FalconFbifTarget::CoherentSysmem)
                    .with_mem_type(FalconFbifMemType::Physical)
            },
        );

        let (mbox0, _) = falcon
            .boot(Some(0), None)
            .inspect_err(|e| dev_err!(dev, "Failed to boot FWSEC firmware: {:?}\n", e))?;
        if mbox0 != 0 {
            dev_err!(dev, "FWSEC firmware returned error {}\n", mbox0);
            Err(EIO)
        } else {
            Ok(())
        }
    }
}

impl FalconFirmware for FwsecFirmwareWithBl {
    type Target = Gsp;

    fn brom_params(&self) -> FalconBromParams {
        self.brom_params.clone()
    }

    fn boot_addr(&self) -> u32 {
        // On V2 platforms, the boot address is extracted from the generic bootloader, because the
        // gbl is what actually copies FWSEC into memory, so that is what needs to be booted.
        self.bootloader.boot_addr()
    }
}

impl FalconPioLoadable for FwsecFirmwareWithBl {
    fn imem_sec_load_params(&self) -> Option<FalconPioImemLoadTarget<'_>> {
        None
    }

    fn imem_ns_load_params(&self) -> Option<FalconPioImemLoadTarget<'_>> {
        Some(self.bootloader.imem_load_params())
    }

    fn dmem_load_params(&self) -> FalconPioDmemLoadTarget<'_> {
        FalconPioDmemLoadTarget {
            data: self.dmem_desc.as_bytes(),
            dst_start: 0,
        }
    }
}
