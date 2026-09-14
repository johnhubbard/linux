// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! The generic falcon bootloader: a small program that the driver loads into a falcon's IMEM by
//! PIO. It reads a descriptor that the driver writes at DMEM offset 0, and loads the image that
//! the descriptor names into IMEM and DMEM by DMA.

use kernel::{
    device,
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
        FalconPioImemLoadTarget, //
    },
    firmware::tlv::{
        request_tlv, //
        Tlv,
    },
    gpu::Chipset,
    num::FromSafeCast, //
};

/// Structure used by the boot-loader to load the rest of the code.
///
/// This has to be filled by the GPU driver and copied into DMEM at offset
/// [`BootloaderDesc.dmem_load_off`].
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

/// The generic falcon bootloader image and its IMEM load parameters.
pub(crate) struct GenericBootloader {
    /// Bootloader code, zero-padded to a whole number of falcon memory blocks.
    ucode: KVec<u8>,
    /// Byte offset in IMEM at which the code is loaded.
    imem_dst_start: u16,
    /// Tag under which the first code block is loaded.
    start_tag: u16,
}

impl GenericBootloader {
    /// Loads the generic bootloader image for `chipset`, placed in the last blocks of an IMEM of
    /// `imem_size` bytes.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if a required TLV field is absent or the image does not fit in IMEM.
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
        let imem_dst_start = imem_size.checked_sub(ucode.len()).ok_or(EINVAL)?;

        Ok(Self {
            ucode,
            imem_dst_start: u16::try_from(imem_dst_start)?,
            start_tag: u16::try_from(tlv.get_u32(b"STRT")?)?,
        })
    }

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
}
