// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! The load-and-execute events of the GSP boot.
//!
//! See `Documentation/gpu/nova/core/falcon.rst`:
//!
//! - "Falcon security levels" describes the Heavy-Secured (HS) mode of a falcon and the boot ROM
//!   that verifies an HS binary. The note in "Root of trust establishment" says why the GSP falcon
//!   stops running GSP-RM, runs an HS binary, and then resumes GSP-RM.
//! - "Falcon memory subsystem and DMA engine" describes the instruction memory (IMEM) and the data
//!   memory (DMEM) of a falcon, and the DMA engine that loads a binary into them. The DMA engine
//!   accesses the framebuffer or system memory through the FBIF (Framebuffer Interface).

use core::array;

use kernel::{
    device,
    dma::DmaAddress,
    prelude::*,
    time::Delta,
    transmute::FromBytes, //
};

use crate::{
    falcon::{
        gsp::Gsp,
        sec2::Sec2,
        Falcon,
        FalconBromParams,
        FalconDmaLoadTarget,
        FalconDmaSource,
        FalconFbifTarget,
        FalconMem, //
    },
    firmware::gen_bootloader::{
        BootloaderDmemDescV2,
        GenericBootloader, //
    },
    gsp::{
        cmdq::gmc::MessageFromGsp,
        GspBootContext, //
    },
    sbuffer::SBufferIter, //
};

/// DMEM virtual address value that means "no virtual address assigned".
const FLCN_DMEM_VA_INVALID: u32 = 0xffff_ffff;

/// `MAILBOX0` value that means that a falcon binary has not started. A binary that reports its
/// status through `MAILBOX0` replaces it.
const FLCN_ERR_BINARY_NOT_STARTED: u32 = 0xfe;

/// Resets the GSP falcon and its DMA-related registers.
pub(super) fn reset_gsp_falcon(gsp_falcon: &Falcon<'_, Gsp>) -> Result {
    gsp_falcon.reset()?;
    gsp_falcon.dma_reset();
    Ok(())
}

/// Resumes GSP-RM on the GSP falcon by starting the SEC2 falcon.
///
/// Resets the GSP falcon and writes `libos_dma_address` to its mailboxes before the start, and
/// writes `bootloader_app_version` to its OS register once the resume has completed.
///
/// The SEC2 falcon must hold a loaded firmware that has not started.
///
/// # Errors
///
/// - `ETIMEDOUT` if the resume has not completed within two seconds.
/// - `EIO` if `MAILBOX0` of the SEC2 falcon is not zero after the resume, or if the RISC-V core of
///   the GSP falcon is not active.
///
/// Errors from the reset of the GSP falcon are propagated as-is.
pub(super) fn resume_gsp_rm(
    dev: &device::Device,
    gsp_falcon: &Falcon<'_, Gsp>,
    sec2_falcon: &Falcon<'_, Sec2>,
    libos_dma_address: DmaAddress,
    bootloader_app_version: u32,
) -> Result {
    // Reset the GSP to prepare it for resuming.
    gsp_falcon.reset()?;

    // Write the libOS DMA address to GSP mailboxes.
    gsp_falcon.write_mailboxes(
        Some(libos_dma_address as u32),
        Some((libos_dma_address >> 32) as u32),
    );

    // Start the SEC2 falcon which will trigger GSP-RM to resume on the GSP.
    sec2_falcon.start()?;

    // Poll until GSP-RM reload/resume has completed (up to 2 seconds).
    gsp_falcon.check_reload_completed(Delta::from_secs(2))?;

    // Verify SEC2 completed successfully by checking its mailbox for errors.
    let mbox0 = sec2_falcon.read_mailbox0();
    if mbox0 != 0 {
        dev_err!(
            dev,
            "SEC2 reported an error while resuming GSP-RM: {:#x}\n",
            mbox0
        );
        return Err(EIO);
    }

    // Configure GSP with the bootloader version.
    gsp_falcon.write_os_version(bootloader_app_version);

    // Verify the GSP's RISC-V core is active indicating successful GSP boot.
    if !gsp_falcon.is_riscv_active() {
        dev_err!(dev, "GSP RISC-V core is not active after resuming GSP-RM\n");
        return Err(EIO);
    }
    Ok(())
}

/// Waits until GSP-RM has suspended the RISC-V core, and then resets the GSP falcon.
fn reset_gsp_falcon_after_suspend(dev: &device::Device, gsp_falcon: &Falcon<'_, Gsp>) -> Result {
    gsp_falcon.wait_for_processor_suspend().inspect_err(|_| {
        dev_err!(
            dev,
            "Timeout waiting for GSP-RM (RISC-V core) to suspend (mbox0={:#x})\n",
            gsp_falcon.read_mailbox0()
        );
    })?;

    reset_gsp_falcon(gsp_falcon)
}

/// Runs the Heavy-Secured (HS) binary of a `GMCAPI_CMD_EXEC_HS_BINARY` event on the falcon core of
/// the GSP falcon, and then resumes GSP-RM.
///
/// `params` describes the binary. The handler:
///
/// 1. Waits until GSP-RM has suspended the RISC-V core, for up to five seconds.
/// 2. Resets the GSP falcon, which selects the falcon core.
/// 3. Loads the code segment into IMEM and the data segment into DMEM, by DMA from the
///    framebuffer.
/// 4. Programs the boot ROM parameters and the boot vector, and writes
///    [`FLCN_ERR_BINARY_NOT_STARTED`] to `MAILBOX0`.
/// 5. Starts the falcon core, and waits until the falcon core halts, for up to two seconds. The
///    binary halts the falcon core when it is done, so the handler learns from the halt that the
///    binary has finished.
/// 6. Logs an error if `MAILBOX0` still holds [`FLCN_ERR_BINARY_NOT_STARTED`], which means that
///    the binary did not start.
/// 7. Resumes GSP-RM on the RISC-V core, via [`resume_gsp_rm`].
///
/// A binary that reports a status writes the status to `MAILBOX0` before it halts. The handler
/// does not check that status, so the result does not depend on it.
///
/// # Errors
///
/// - `EINVAL` if the engine ID mask or the ucode ID of `params` does not fit its
///   [`FalconBromParams`] field.
/// - `ETIMEDOUT` if GSP-RM has not suspended the RISC-V core within five seconds, or if the
///   falcon core has not halted within two seconds of its start.
///
/// Errors from the reset of the GSP falcon, the DMA transfers and the resume of GSP-RM are
/// propagated as-is.
#[expect(dead_code)]
fn handle_hs_binary(
    ctx: &GspBootContext<'_, '_>,
    libos_dma_address: DmaAddress,
    bootloader_app_version: u32,
    params: &HsBinaryParams,
) -> Result {
    let dev = ctx.dev();
    let gsp_falcon = ctx.gsp_falcon;
    let brom_params = FalconBromParams {
        pkc_data_offset: params.hs_sig_dmem_addr,
        engine_id_mask: u16::try_from(params.engine_id_mask).map_err(|_| EINVAL)?,
        ucode_id: u8::try_from(params.ucode_id).map_err(|_| EINVAL)?,
    };

    reset_gsp_falcon_after_suspend(dev, gsp_falcon)?;

    gsp_falcon.dma_wr(
        FalconDmaSource::Framebuffer {
            base: params.imem_dma_base,
        },
        FalconMem::ImemSecure,
        FalconDmaLoadTarget {
            src_start: params.imem_va,
            dst_start: params.imem_pa,
            len: params.imem_size,
        },
    )?;

    // A data segment with a virtual address is tagged with it. One without starts at the DMA base.
    let tag_dmem = params.dmem_va != FLCN_DMEM_VA_INVALID;

    gsp_falcon.dma_wr(
        FalconDmaSource::Framebuffer {
            base: params.dmem_dma_base,
        },
        FalconMem::Dmem { tag: tag_dmem },
        FalconDmaLoadTarget {
            src_start: if tag_dmem { params.dmem_va } else { 0 },
            dst_start: params.dmem_pa,
            len: params.dmem_size,
        },
    )?;

    gsp_falcon.program_brom_and_bootvec(&brom_params, params.imem_va);

    let (mbox0, _) = gsp_falcon
        .boot(Some(FLCN_ERR_BINARY_NOT_STARTED), None)
        .inspect_err(|_| {
            dev_err!(
                dev,
                "Timeout waiting for HS binary to halt (mbox0={:#x})\n",
                gsp_falcon.read_mailbox0()
            );
        })?;
    if mbox0 == FLCN_ERR_BINARY_NOT_STARTED {
        dev_err!(dev, "HS binary did not start\n");
    } else {
        dev_dbg!(dev, "HS binary halted with mbox0={:#x}\n", mbox0);
    }

    resume_gsp_rm(
        dev,
        gsp_falcon,
        ctx.sec2_falcon,
        libos_dma_address,
        bootloader_app_version,
    )
}

/// Runs the image of a `GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER` event on the falcon core of the GSP
/// falcon, through the generic falcon bootloader, and then resumes GSP-RM.
///
/// `params` holds the descriptor of the image. The generic falcon bootloader loads and starts the
/// image that the descriptor names, per [`GenericBootloader::run`]. The handler:
///
/// 1. Checks that the descriptor in `params` has the size of [`BootloaderDmemDescV2`], and gets
///    the FBIF (Framebuffer Interface) target that `params` selects.
/// 2. Waits until GSP-RM has suspended the RISC-V core, for up to five seconds.
/// 3. Resets the GSP falcon, which selects the falcon core.
/// 4. Runs `bootloader` with the descriptor and with [`FLCN_ERR_BINARY_NOT_STARTED`] in
///    `MAILBOX0`, and waits until the falcon core halts, for up to two seconds. The image halts
///    the falcon core when it is done, so the handler learns from the halt that the image has
///    finished.
/// 5. Logs an error if `MAILBOX0` still holds [`FLCN_ERR_BINARY_NOT_STARTED`], which means that
///    the image did not start.
/// 6. Resumes GSP-RM on the RISC-V core, via [`resume_gsp_rm`].
///
/// An image that reports a status writes the status to `MAILBOX0` before it halts. The handler
/// does not check that status, so the result does not depend on it.
///
/// # Errors
///
/// - `EINVAL` if `bootloader` is `None`, if the descriptor size in `params` is not the size of
///   [`BootloaderDmemDescV2`], or if the address space and the cache attribute in `params` select
///   no FBIF target.
/// - `ETIMEDOUT` if GSP-RM has not suspended the RISC-V core within five seconds, or if the
///   falcon core has not halted within two seconds of its start.
///
/// Errors from the reset of the GSP falcon, the run of the bootloader and the resume of GSP-RM
/// are propagated as-is.
#[expect(dead_code)]
fn handle_generic_bootloader(
    ctx: &GspBootContext<'_, '_>,
    bootloader: Option<&GenericBootloader>,
    libos_dma_address: DmaAddress,
    bootloader_app_version: u32,
    params: &GenericBootloaderParams,
) -> Result {
    /// Expected size of the descriptor, in bytes.
    const DMEM_DESC_SIZE: u32 = cv!(size_of::<BootloaderDmemDescV2>());

    let dev = ctx.dev();
    let gsp_falcon = ctx.gsp_falcon;
    let Some(bootloader) = bootloader else {
        dev_err!(
            dev,
            "No generic bootloader for the load-and-execute event\n"
        );
        return Err(EINVAL);
    };

    if params.dmem_desc_size != DMEM_DESC_SIZE {
        dev_err!(
            dev,
            "Generic bootloader descriptor is {} bytes, expected {}\n",
            params.dmem_desc_size,
            DMEM_DESC_SIZE
        );
        return Err(EINVAL);
    }

    let fbif_target = params.fbif_target()?;

    reset_gsp_falcon_after_suspend(dev, gsp_falcon)?;

    let (mbox0, _) = bootloader
        .run(
            gsp_falcon,
            &params.dmem_desc,
            fbif_target,
            FLCN_ERR_BINARY_NOT_STARTED,
        )
        .inspect_err(|e| {
            dev_err!(
                dev,
                "Failed to run the generic bootloader: {:?} (mbox0={:#x})\n",
                e,
                gsp_falcon.read_mailbox0()
            );
        })?;
    if mbox0 == FLCN_ERR_BINARY_NOT_STARTED {
        dev_err!(dev, "Image of the generic bootloader did not start\n");
    } else {
        dev_dbg!(
            dev,
            "Image of the generic bootloader halted with mbox0={:#x}\n",
            mbox0
        );
    }

    resume_gsp_rm(
        dev,
        gsp_falcon,
        ctx.sec2_falcon,
        libos_dma_address,
        bootloader_app_version,
    )
}

/// Payload of a `GMCAPI_CMD_EXEC_HS_BINARY` event, which describes the HS binary to run.
#[repr(C)]
struct HsBinaryParams {
    /// DMA base of the code segment, a physical address in the framebuffer. The code segment starts
    /// at `imem_dma_base + imem_va`.
    imem_dma_base: u64,
    /// DMA base of the data segment, a physical address in the framebuffer. The data segment starts
    /// at `dmem_dma_base + dmem_va`, or at `dmem_dma_base` when the data segment has no virtual
    /// address.
    dmem_dma_base: u64,
    _reserved64: [u64; 2],
    /// Virtual address of the code segment. The binary starts executing at this address.
    imem_va: u32,
    /// Physical address in IMEM at which the code segment is loaded.
    imem_pa: u32,
    /// Size in bytes of the code segment.
    imem_size: u32,
    /// Virtual address of the data segment, or [`FLCN_DMEM_VA_INVALID`] if the data segment has
    /// no virtual address. The DMA transfer writes no tags in that case.
    dmem_va: u32,
    /// Physical address in DMEM at which the data segment is loaded.
    dmem_pa: u32,
    /// Size in bytes of the data segment.
    dmem_size: u32,
    /// Physical address in DMEM of the signature of the binary.
    hs_sig_dmem_addr: u32,
    /// Mask of the falcon engines on which the binary can run, with one bit per engine.
    engine_id_mask: u32,
    /// ID number of the binary. Each ID has a fuse register that holds a version number. The boot
    /// ROM uses the ID and that version number as inputs of the signature verification.
    ucode_id: u32,
    _reserved32: [u32; 3],
}

// SAFETY: This struct only contains integer types for which all bit patterns are valid.
unsafe impl FromBytes for HsBinaryParams {}

impl MessageFromGsp for HsBinaryParams {
    type InitError = Error;

    fn read(payload: &mut SBufferIter<array::IntoIter<&[u8], 2>>) -> Result<Self> {
        let mut bytes = [0u8; size_of::<Self>()];

        payload.read_exact(&mut bytes)?;

        Self::from_bytes_copy(&bytes).ok_or(EINVAL)
    }
}

/// Payload of a `GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER` event, which describes the image for the
/// generic falcon bootloader to run.
#[repr(C)]
struct GenericBootloaderParams {
    /// Descriptor of the image, which the bootloader reads from DMEM.
    dmem_desc: BootloaderDmemDescV2,
    /// Size in bytes of the descriptor, as GSP-RM defines it.
    dmem_desc_size: u32,
    /// Address space of the image: [`Self::ADDR_SYSMEM`] for system memory, or
    /// [`Self::ADDR_FBMEM`] for the framebuffer.
    addr_space: u32,
    /// CPU cache attribute of an image in system memory: [`Self::NV_MEMORY_CACHED`] or
    /// [`Self::NV_MEMORY_UNCACHED`].
    cpu_cache_attrib: u32,
    _reserved: [u32; 4],
}

impl GenericBootloaderParams {
    const ADDR_SYSMEM: u32 = 1;
    const ADDR_FBMEM: u32 = 2;
    const NV_MEMORY_CACHED: u32 = 0;
    const NV_MEMORY_UNCACHED: u32 = 1;

    /// Returns the FBIF target that `addr_space` and `cpu_cache_attrib` select.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the pair selects no FBIF target.
    fn fbif_target(&self) -> Result<FalconFbifTarget> {
        match (self.addr_space, self.cpu_cache_attrib) {
            (Self::ADDR_FBMEM, _) => Ok(FalconFbifTarget::LocalFb),
            (Self::ADDR_SYSMEM, Self::NV_MEMORY_CACHED) => Ok(FalconFbifTarget::CoherentSysmem),
            (Self::ADDR_SYSMEM, Self::NV_MEMORY_UNCACHED) => {
                Ok(FalconFbifTarget::NoncoherentSysmem)
            }
            _ => Err(EINVAL),
        }
    }
}

// SAFETY: The nested descriptor is `FromBytes`, and every other field is an integer type for
// which all bit patterns are valid.
unsafe impl FromBytes for GenericBootloaderParams {}

impl MessageFromGsp for GenericBootloaderParams {
    type InitError = Error;

    fn read(payload: &mut SBufferIter<array::IntoIter<&[u8], 2>>) -> Result<Self> {
        let mut bytes = [0u8; size_of::<Self>()];

        payload.read_exact(&mut bytes)?;

        Self::from_bytes_copy(&bytes).ok_or(EINVAL)
    }
}
