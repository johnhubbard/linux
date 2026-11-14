// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! The core reset and the core resume of the GSP falcon.

use kernel::{
    device,
    dma::DmaAddress,
    prelude::*,
    time::Delta, //
};

use crate::falcon::{
    gsp::Gsp,
    sec2::Sec2,
    Falcon, //
};

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
        dev_err!(dev, "Sequencer: sec2 errors: {:?}\n", mbox0);
        return Err(EIO);
    }

    // Configure GSP with the bootloader version.
    gsp_falcon.write_os_version(bootloader_app_version);

    // Verify the GSP's RISC-V core is active indicating successful GSP boot.
    if !gsp_falcon.is_riscv_active() {
        dev_err!(dev, "Sequencer: RISC-V core is not active\n");
        return Err(EIO);
    }
    Ok(())
}
