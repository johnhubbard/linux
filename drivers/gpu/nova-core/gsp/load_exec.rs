// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! The load-and-execute events of the GSP boot.
//!
//! GSP-RM runs on the GSP falcon, so it cannot run another binary there by itself. It suspends its
//! RISC-V core and raises a load-and-execute event instead. The handler of the event resets the
//! falcon with [`core_reset`], runs the binary, and resumes GSP-RM with [`core_resume`].

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
        FalconMem,
        FLCN_ERR_BINARY_NOT_STARTED, //
    },
    gsp::{
        cmdq::gmc::MessageFromGsp,
        GspBootContext, //
    },
    sbuffer::SBufferIter, //
};

/// DMEM virtual address value that means "no virtual address assigned".
const FLCN_DMEM_VA_INVALID: u32 = 0xffff_ffff;

/// Resets the GSP falcon and its DMA-related registers.
pub(super) fn core_reset(gsp_falcon: &Falcon<'_, Gsp>) -> Result {
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
pub(super) fn core_resume(
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
            "SEC2 reported error during core resume: {:#x}\n",
            mbox0
        );
        return Err(EIO);
    }

    // Configure GSP with the bootloader version.
    gsp_falcon.write_os_version(bootloader_app_version);

    // Verify the GSP's RISC-V core is active indicating successful GSP boot.
    if !gsp_falcon.is_riscv_active() {
        dev_err!(dev, "GSP RISC-V core is not active after core resume\n");
        return Err(EIO);
    }
    Ok(())
}

/// Waits until GSP-RM has suspended the RISC-V core, and then runs the core reset.
fn reset_gsp_falcon_after_suspend(dev: &device::Device, gsp_falcon: &Falcon<'_, Gsp>) -> Result {
    gsp_falcon.wait_for_processor_suspend().inspect_err(|_| {
        dev_err!(
            dev,
            "Timeout waiting for GSP suspend (mbox0={:#x})\n",
            gsp_falcon.read_mailbox0()
        );
    })?;

    core_reset(gsp_falcon)
}

/// Runs the Heavy-Secured (HS) binary of a `GMCAPI_CMD_EXEC_HS_BINARY` event on the GSP falcon,
/// and then resumes GSP-RM.
///
/// `payload` is the payload of the event. The handler does not check the status of the binary: it
/// returns `Ok` whatever `MAILBOX0` holds when the falcon halts, and logs an error if `MAILBOX0`
/// still holds [`FLCN_ERR_BINARY_NOT_STARTED`].
///
/// # Errors
///
/// - `EINVAL` if the payload is shorter than the parameter block, or if the engine ID mask or the
///   ucode ID does not fit its boot ROM (BROM) register field.
/// - `ETIMEDOUT` if GSP-RM has not suspended the RISC-V core within five seconds, or if the GSP
///   falcon has not halted within two seconds of starting the binary.
///
/// Errors from the core reset, the DMA transfers and the core resume are propagated as-is.
#[expect(dead_code)]
pub(super) fn handle_hs_binary(
    ctx: &GspBootContext<'_, '_>,
    libos_dma_address: DmaAddress,
    bootloader_app_version: u32,
    payload: (&[u8], &[u8]),
) -> Result {
    let dev = ctx.dev();
    let gsp_falcon = ctx.gsp_falcon;
    let params = HsBinaryParams::read(&mut SBufferIter::new_reader([payload.0, payload.1]))?;

    reset_gsp_falcon_after_suspend(dev, gsp_falcon)?;

    if params.ucode_imem_size > 0 {
        gsp_falcon.dma_wr(
            FalconDmaSource::Framebuffer {
                base: params.imem_phys_addr,
            },
            FalconMem::ImemSecure,
            FalconDmaLoadTarget {
                src_start: params.ucode_imem_va,
                dst_start: params.ucode_imem_pa,
                len: params.ucode_imem_size,
            },
        )?;
    }

    if params.ucode_dmem_size > 0 {
        // A DMEM image with a virtual address is tagged with it. One without starts at the DMA
        // base.
        let tag_dmem = params.ucode_dmem_va != FLCN_DMEM_VA_INVALID;

        gsp_falcon.dma_wr(
            FalconDmaSource::Framebuffer {
                base: params.dmem_phys_addr,
            },
            FalconMem::Dmem { tag: tag_dmem },
            FalconDmaLoadTarget {
                src_start: if tag_dmem { params.ucode_dmem_va } else { 0 },
                dst_start: params.ucode_dmem_pa,
                len: params.ucode_dmem_size,
            },
        )?;
    }

    gsp_falcon.program_brom_and_bootvec(
        &FalconBromParams {
            pkc_data_offset: params.hs_sig_dmem_addr,
            engine_id_mask: u16::try_from(params.engine_id_mask).map_err(|_| EINVAL)?,
            ucode_id: u8::try_from(params.ucode_id).map_err(|_| EINVAL)?,
        },
        params.ucode_imem_va,
    );

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

    core_resume(
        dev,
        gsp_falcon,
        ctx.sec2_falcon,
        libos_dma_address,
        bootloader_app_version,
    )
}

/// Payload of a `GMCAPI_CMD_EXEC_HS_BINARY` event.
///
/// GSP-RM has placed the binary in the framebuffer before it raises the event.
///
/// The layout is OpenRM's `GspLoadExecHsBinaryParams`.
#[repr(C)]
struct HsBinaryParams {
    /// DMA base of the code. The code is at `imem_phys_addr + ucode_imem_va`.
    imem_phys_addr: u64,
    /// DMA base of the data. The data is at `dmem_phys_addr + ucode_dmem_va`, or at
    /// `dmem_phys_addr` when `ucode_dmem_va` is [`FLCN_DMEM_VA_INVALID`].
    dmem_phys_addr: u64,
    _reserved64: [u64; 2],
    ucode_imem_va: u32,
    ucode_imem_pa: u32,
    ucode_imem_size: u32,
    ucode_dmem_va: u32,
    ucode_dmem_pa: u32,
    ucode_dmem_size: u32,
    hs_sig_dmem_addr: u32,
    engine_id_mask: u32,
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
