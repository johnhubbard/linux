// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! The generic falcon bootloader: a small program that the driver loads into a falcon's IMEM by
//! PIO. It reads a descriptor that the driver writes at DMEM offset 0, and loads the image that
//! the descriptor names into IMEM and DMEM by DMA.

use kernel::{
    device,
    io::{
        register::Array,
        Io, //
    },
    prelude::*,
    ptr::{
        Alignable,
        Alignment, //
    },
    transmute::AsBytes, //
};

use crate::{
    falcon::{
        self,
        gsp::Gsp,
        Falcon,
        FalconBromParams,
        FalconDmaLoadTarget,
        FalconFbifMemType,
        FalconFbifTarget,
        FalconFirmware,
        FalconPioDmemLoadTarget,
        FalconPioImemLoadTarget,
        FalconPioLoadable, //
    },
    firmware::tlv::{
        request_tlv, //
        Tlv,
    },
    gpu::Chipset,
    num::FromSafeCast, //
    regs,
};

/// Structure used by the boot-loader to load the rest of the code.
///
/// This has to be filled by the GPU driver and copied into DMEM at offset
/// [`BootloaderDesc.dmem_load_off`].
#[repr(C, packed)]
#[derive(Debug, Clone)]
pub(crate) struct BootloaderDmemDescV2 {
    /// Reserved, should always be first element.
    reserved: [u32; 4],
    /// 16B signature for secure code, 0s if no secure code.
    signature: [u32; 4],
    /// DMA context used by the bootloader while loading code/data.
    ctx_dma: u32,
    /// 256B-aligned physical FB address where code is located.
    code_dma_base: u64,
    /// Offset from `code_dma_base` where the non-secure code is located.
    ///
    /// Also used as destination IMEM offset of non-secure code as the DMA firmware object is
    /// expected to be a mirror image of its loaded state.
    ///
    /// Must be multiple of 256.
    non_sec_code_off: u32,
    /// Size of the non-secure code part.
    non_sec_code_size: u32,
    /// Offset from `code_dma_base` where the secure code is located (must be multiple of 256).
    ///
    /// Also used as destination IMEM offset of secure code as the DMA firmware object is expected
    /// to be a mirror image of its loaded state.
    ///
    /// Must be multiple of 256.
    sec_code_off: u32,
    /// Size of the secure code part.
    sec_code_size: u32,
    /// Code entry point invoked by the bootloader after code is loaded.
    code_entry_point: u32,
    /// 256B-aligned physical FB address where data is located.
    data_dma_base: u64,
    /// Size of data block (should be multiple of 256B).
    data_size: u32,
    /// Number of arguments to be passed to the target firmware being loaded.
    argc: u32,
    /// Arguments to be passed to the target firmware being loaded.
    argv: u32,
}
// SAFETY: This struct doesn't contain uninitialized bytes and doesn't have interior mutability.
unsafe impl AsBytes for BootloaderDmemDescV2 {}

impl BootloaderDmemDescV2 {
    /// Builds the descriptor of an image whose code is at `code_dma_base` and whose data is at
    /// `data_dma_base`.
    ///
    /// `imem_ns` and `imem_sec` locate the non-secure and the secure code within the code image,
    /// and `dmem` gives the size of the data. The code image must be a mirror image of the loaded
    /// IMEM.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if `dmem.dst_start` is not 0, because the bootloader copies the data to the start
    ///   of DMEM.
    pub(crate) fn new(
        code_dma_base: u64,
        imem_ns: FalconDmaLoadTarget,
        imem_sec: FalconDmaLoadTarget,
        data_dma_base: u64,
        dmem: FalconDmaLoadTarget,
    ) -> Result<Self> {
        // Bootloader payload is in non-coherent system memory.
        const FALCON_DMAIDX_PHYS_SYS_NCOH: u32 = 4;

        // The bootloader does not have a data destination offset field and copies the data at
        // the start of DMEM, so it can only be used if the destination offset of the firmware
        // is 0.
        if dmem.dst_start != 0 {
            return Err(EINVAL);
        }

        Ok(Self {
            reserved: [0; 4],
            signature: [0; 4],
            ctx_dma: FALCON_DMAIDX_PHYS_SYS_NCOH,
            code_dma_base,
            // `dst_start` is also valid as the source offset since the firmware DMA object is
            // a mirror image of the target IMEM layout.
            non_sec_code_off: imem_ns.dst_start,
            non_sec_code_size: imem_ns.len,
            // `dst_start` is also valid as the source offset since the firmware DMA object is
            // a mirror image of the target IMEM layout.
            sec_code_off: imem_sec.dst_start,
            sec_code_size: imem_sec.len,
            code_entry_point: 0,
            data_dma_base,
            data_size: dmem.len,
            argc: 0,
            argv: 0,
        })
    }

    /// Returns the DMA context that the bootloader loads the code and data through.
    pub(crate) fn ctx_dma(&self) -> u32 {
        self.ctx_dma
    }
}

/// The generic falcon bootloader image and its IMEM load parameters.
pub(crate) struct GenericBootloader {
    /// Code of the bootloader to be loaded into non-secure IMEM.
    ucode: KVec<u8>,
    /// Range-validated start offset of the firmware code in IMEM.
    imem_dst_start: u16,
    /// Range-validated `desc.start_tag`.
    start_tag: u16,
}

impl GenericBootloader {
    /// Loads the generic bootloader image for `chipset`, placed in the last blocks of an IMEM of
    /// `imem_size` bytes.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if a required TLV field is absent.
    /// - `EOVERFLOW` if the image does not fit in IMEM.
    /// - `ENOMEM` if the padded copy of the code cannot be allocated.
    pub(crate) fn new(
        dev: &device::Device<device::Bound>,
        chipset: Chipset,
        imem_size: usize,
    ) -> Result<Self> {
        let fw = request_tlv(dev, chipset, "gen_bootloader")?;
        let tlv = Tlv::new(fw.data())?;
        dev_dbg!(
            dev,
            "loaded generic bootloader firmware v{}\n",
            tlv.get_string(b"VERS")?
        );

        let ucode = {
            let blob = tlv.get_bytes(b"BLOB")?;
            let code_size = usize::from_safe_cast(tlv.get_u32(b"CDSZ")?);
            let code = blob.get(..code_size).ok_or(EINVAL)?;
            let aligned_code_size = code_size
                .align_up(Alignment::new::<{ falcon::MEM_BLOCK_ALIGNMENT }>())
                .ok_or(EINVAL)?;

            let mut ucode = KVec::with_capacity(aligned_code_size, GFP_KERNEL)?;
            ucode.extend_from_slice(code, GFP_KERNEL)?;
            ucode.resize(aligned_code_size, 0, GFP_KERNEL)?;

            ucode
        };

        // The top of IMEM, above the blocks that the bootloader loads the image into.
        let imem_dst_start = imem_size.checked_sub(ucode.len()).ok_or(EOVERFLOW)?;

        let start_tag = u16::try_from(tlv.get_u32(b"STRT")?)?;

        Ok(Self {
            ucode,
            imem_dst_start: u16::try_from(imem_dst_start)?,
            start_tag,
        })
    }

    /// Returns the start address of the bootloader.
    fn boot_addr(&self) -> u32 {
        u32::from(self.start_tag) << 8
    }

    /// Returns the PIO parameters that place this bootloader in non-secure IMEM.
    fn imem_load_params(&self) -> FalconPioImemLoadTarget<'_> {
        FalconPioImemLoadTarget {
            data: self.ucode.as_ref(),
            dst_start: self.imem_dst_start,
            secure: false,
            start_tag: self.start_tag,
        }
    }

    /// Loads this bootloader and `dmem_desc` into `falcon` by PIO, and runs the bootloader.
    ///
    /// The DMA context that `dmem_desc` names is programmed for physical addresses in `target`,
    /// the falcon's BROM parameters are set from `brom_params`, and `mbox0` is written to
    /// `MAILBOX0` before the falcon starts.
    ///
    /// Returns the `MAILBOX0` and `MAILBOX1` values once the falcon halts.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the DMA context that `dmem_desc` names is not a `TRANSCFG` slot of the
    ///   falcon.
    ///
    /// Errors from the PIO load and from [`Falcon::boot`] are propagated as-is.
    pub(crate) fn run(
        &self,
        falcon: &Falcon<'_, Gsp>,
        dmem_desc: &BootloaderDmemDescV2,
        brom_params: &FalconBromParams,
        target: FalconFbifTarget,
        mbox0: u32,
    ) -> Result<(u32, u32)> {
        falcon.pio_load(&GenericBootloaderLoad {
            bootloader: self,
            dmem_desc,
            brom_params,
        })?;

        falcon.pfalcon.update(
            regs::NV_PFALCON_FBIF_TRANSCFG::try_at(usize::from_safe_cast(dmem_desc.ctx_dma()))
                .ok_or(EINVAL)?,
            |v| {
                v.with_target(target)
                    .with_mem_type(FalconFbifMemType::Physical)
            },
        );

        falcon.boot(Some(mbox0), None)
    }
}

/// The bootloader paired with a descriptor, as the firmware that [`Falcon::pio_load`] loads: the
/// bootloader into non-secure IMEM, and the descriptor into DMEM at offset 0.
struct GenericBootloaderLoad<'a> {
    bootloader: &'a GenericBootloader,
    dmem_desc: &'a BootloaderDmemDescV2,
    brom_params: &'a FalconBromParams,
}

impl FalconFirmware for GenericBootloaderLoad<'_> {
    type Target = Gsp;

    fn brom_params(&self) -> FalconBromParams {
        self.brom_params.clone()
    }

    fn boot_addr(&self) -> u32 {
        self.bootloader.boot_addr()
    }
}

impl FalconPioLoadable for GenericBootloaderLoad<'_> {
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
