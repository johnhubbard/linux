// SPDX-License-Identifier: GPL-2.0

//! The generic falcon bootloader.
//!
//! A small program loaded into falcon IMEM using PIO, which then DMAs a larger image into IMEM and
//! DMEM from a descriptor the driver leaves in DMEM at offset 0. Open RM's
//! `ksec2GetGenericBlUcode` supplies the same image for both SEC2 and GSP.

use kernel::{
    device,
    prelude::*,
    ptr::{
        Alignable,
        Alignment, //
    },
    transmute::{
        AsBytes,
        FromBytes, //
    },
};

use crate::{
    falcon::{
        self,
        gsp::Gsp,
        Falcon,
        FalconBromParams,
        FalconEngine,
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
};

/// Structure the generic bootloader reads from DMEM offset 0 to find the image it must load.
///
/// Mirrors Open RM's `RM_FLCN_BL_DMEM_DESC`. The driver fills one in when it loads a firmware
/// through the bootloader, and GSP-RM sends one in a load-and-execute event.
#[repr(C, packed)]
#[derive(Debug, Clone)]
pub(crate) struct BootloaderDmemDescV2 {
    /// Reserved, should always be first element.
    pub(crate) reserved: [u32; 4],
    /// 16B signature for secure code, 0s if no secure code.
    pub(crate) signature: [u32; 4],
    /// DMA context used by the bootloader while loading code/data.
    pub(crate) ctx_dma: u32,
    /// 256B-aligned physical FB address where code is located.
    pub(crate) code_dma_base: u64,
    /// Offset from `code_dma_base` where the non-secure code is located.
    ///
    /// Also used as destination IMEM offset of non-secure code as the DMA firmware object is
    /// expected to be a mirror image of its loaded state.
    ///
    /// Must be multiple of 256.
    pub(crate) non_sec_code_off: u32,
    /// Size of the non-secure code part.
    pub(crate) non_sec_code_size: u32,
    /// Offset from `code_dma_base` where the secure code is located (must be multiple of 256).
    ///
    /// Also used as destination IMEM offset of secure code as the DMA firmware object is expected
    /// to be a mirror image of its loaded state.
    ///
    /// Must be multiple of 256.
    pub(crate) sec_code_off: u32,
    /// Size of the secure code part.
    pub(crate) sec_code_size: u32,
    /// Code entry point invoked by the bootloader after code is loaded.
    pub(crate) code_entry_point: u32,
    /// 256B-aligned physical FB address where data is located.
    pub(crate) data_dma_base: u64,
    /// Size of data block (should be multiple of 256B).
    pub(crate) data_size: u32,
    /// Number of arguments to be passed to the target firmware being loaded.
    pub(crate) argc: u32,
    /// Arguments to be passed to the target firmware being loaded.
    pub(crate) argv: u32,
}

// SAFETY: This struct doesn't contain uninitialized bytes and doesn't have interior mutability.
unsafe impl AsBytes for BootloaderDmemDescV2 {}

// SAFETY: This struct only contains integer types for which all bit patterns are valid.
unsafe impl FromBytes for BootloaderDmemDescV2 {}

/// The generic falcon bootloader image and the IMEM placement it was loaded for.
pub(crate) struct GenericBootloader {
    /// Bootloader code, zero-padded to a whole number of falcon memory blocks.
    ucode: KVec<u8>,
    /// Byte offset in IMEM the code is loaded at.
    imem_dst_start: u16,
    /// Tag the first code block is loaded under.
    start_tag: u16,
}

impl GenericBootloader {
    /// Loads the generic bootloader image for `chipset`, placed in the last blocks of `falcon`'s
    /// IMEM so the image it goes on to load has the rest to itself.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if a required TLV field is absent or the image does not fit in IMEM.
    /// - `ENOMEM` if the padded copy of the code cannot be allocated.
    pub(crate) fn new<E: FalconEngine + 'static>(
        dev: &device::Device<device::Bound>,
        chipset: Chipset,
        falcon: &Falcon<'_, E>,
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

        let imem_dst_start = falcon.imem_size().checked_sub(ucode.len()).ok_or(EINVAL)?;

        Ok(Self {
            ucode,
            imem_dst_start: u16::try_from(imem_dst_start)?,
            start_tag: u16::try_from(tlv.get_u32(b"STRT")?)?,
        })
    }

    /// Returns the address the falcon must boot from to run this bootloader.
    pub(crate) fn boot_addr(&self) -> u32 {
        u32::from(self.start_tag) << 8
    }

    /// Returns the PIO parameters that place this bootloader in non-secure IMEM.
    pub(crate) fn imem_load_params(&self) -> FalconPioImemLoadTarget<'_> {
        FalconPioImemLoadTarget {
            data: self.ucode.as_ref(),
            dst_start: self.imem_dst_start,
            secure: false,
            start_tag: self.start_tag,
        }
    }

    /// Pairs this bootloader with the descriptor of the image it is to load, giving something
    /// [`Falcon::pio_load`] accepts.
    pub(crate) fn with_descriptor<'a>(
        &'a self,
        dmem_desc: &'a BootloaderDmemDescV2,
    ) -> GenericBootloaderLoad<'a> {
        GenericBootloaderLoad {
            bootloader: self,
            dmem_desc,
        }
    }
}

/// The generic bootloader together with the descriptor it reads from DMEM offset 0.
pub(crate) struct GenericBootloaderLoad<'a> {
    bootloader: &'a GenericBootloader,
    dmem_desc: &'a BootloaderDmemDescV2,
}

impl FalconFirmware for GenericBootloaderLoad<'_> {
    type Target = Gsp;

    fn brom_params(&self) -> FalconBromParams {
        // The bootloader is not signed. Every chipset that loads it this way uses a falcon HAL
        // whose BROM programming is a no-op, so these values are never written to hardware.
        FalconBromParams {
            pkc_data_offset: 0,
            engine_id_mask: 0,
            ucode_id: 0,
        }
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
