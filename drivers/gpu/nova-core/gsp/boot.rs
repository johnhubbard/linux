// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use kernel::{
    device,
    io::poll::read_poll_timeout,
    num::Bounded,
    prelude::*,
    time::Delta,
    transmute::{
        AsBytes,
        FromBytes, //
    },
    types::ScopeGuard, //
};

use crate::{
    falcon::{
        gsp::Gsp,
        Falcon,
        FalconBromParams,
        FalconDmaLoadTarget,
        FalconDmaSource,
        FalconFbifEngineIdFlag,
        FalconFbifMemType,
        FalconFbifTarget,
        FalconMem,
        FLCN_ERR_BINARY_NOT_STARTED, //
    },
    firmware::gsp::GspFirmware,
    gsp::{
        cmdq::Cmdq,
        commands, //
    },
    sbuffer::SBufferIter, //
};

/// DMEM virtual address value that means "no virtual address assigned".
const FLCN_DMEM_VA_INVALID: u32 = 0xffff_ffff;

/// The GSP falcon and device that the load-and-execute event handlers share.
struct LoadExecContext<'a, 'gpu> {
    gsp_falcon: &'a Falcon<'gpu, Gsp>,
    dev: &'a device::Device,
}

#[expect(dead_code)]
impl LoadExecContext<'_, '_> {
    /// Waits until GSP-RM has suspended the RISC-V core, then resets the GSP falcon and its DMA
    /// registers.
    ///
    /// # Errors
    ///
    /// - `ETIMEDOUT` if the core has not suspended within two seconds.
    ///
    /// Errors from the falcon reset are propagated as-is.
    fn reset_gsp_falcon_after_suspend(&self) -> Result {
        self.gsp_falcon
            .wait_for_processor_suspend()
            .inspect_err(|_| {
                dev_err!(
                    self.dev,
                    "Timeout waiting for GSP suspend (mbox0={:#x})\n",
                    self.gsp_falcon.read_mailbox0()
                );
            })?;

        self.gsp_falcon.reset()?;
        self.gsp_falcon.dma_reset();

        Ok(())
    }

    /// Runs a Heavy-Secured (HS) binary on the GSP falcon, as a `GMCAPI_CMD_EXEC_HS_BINARY` event
    /// requests.
    ///
    /// A successful return means that the GSP falcon has halted and GSP-RM is not running. The
    /// handler returns `Ok` whatever `MAILBOX0` holds when the falcon halts, and logs an error when
    /// `MAILBOX0` still holds [`FLCN_ERR_BINARY_NOT_STARTED`].
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the payload is shorter than the parameter block, or the engine ID mask or the
    ///   ucode ID does not fit its boot ROM (BROM) register field.
    /// - `ETIMEDOUT` if the RISC-V core does not suspend within two seconds, or the GSP falcon does
    ///   not halt within two seconds of starting the binary.
    ///
    /// Errors from the falcon reset and the DMA transfers are propagated as-is.
    fn handle_load_exec_hs_binary(&self, payload: (&[u8], &[u8])) -> Result {
        let gsp_falcon = self.gsp_falcon;
        let params = read_params::<HsBinaryParams>(payload)?;

        self.reset_gsp_falcon_after_suspend()?;

        gsp_falcon.with_fbif_aperture(
            HsBinaryParams::APERTURE,
            |v| {
                v.with_target(FalconFbifTarget::LocalFb)
                    .with_mem_type(FalconFbifMemType::Physical)
                    .with_engine_id_flag(FalconFbifEngineIdFlag::Bar2Fn0)
            },
            || {
                if params.ucode_imem_size > 0 {
                    gsp_falcon.dma_wr(
                        FalconDmaSource::Framebuffer {
                            base: params.imem_phys_addr,
                        },
                        HsBinaryParams::APERTURE,
                        FalconMem::ImemSecure,
                        FalconDmaLoadTarget {
                            src_start: params.ucode_imem_va,
                            dst_start: params.ucode_imem_pa,
                            len: params.ucode_imem_size,
                        },
                    )?;
                }

                if params.ucode_dmem_size > 0 {
                    // A DMEM image with a virtual address is tagged with it. One without starts at
                    // the DMA base.
                    let tag_dmem = params.ucode_dmem_va != FLCN_DMEM_VA_INVALID;

                    gsp_falcon.dma_wr(
                        FalconDmaSource::Framebuffer {
                            base: params.dmem_phys_addr,
                        },
                        HsBinaryParams::APERTURE,
                        FalconMem::Dmem { tag: tag_dmem },
                        FalconDmaLoadTarget {
                            src_start: if tag_dmem { params.ucode_dmem_va } else { 0 },
                            dst_start: params.ucode_dmem_pa,
                            len: params.ucode_dmem_size,
                        },
                    )?;
                }

                gsp_falcon.program_brom(&FalconBromParams {
                    pkc_data_offset: params.hs_sig_dmem_addr,
                    engine_id_mask: u16::try_from(params.engine_id_mask).map_err(|_| EINVAL)?,
                    ucode_id: u8::try_from(params.ucode_id).map_err(|_| EINVAL)?,
                });

                gsp_falcon.write_bootvec(params.ucode_imem_va);

                let (mbox0, _) = gsp_falcon
                    .boot(Some(FLCN_ERR_BINARY_NOT_STARTED), None)
                    .inspect_err(|_| {
                        dev_err!(
                            self.dev,
                            "Timeout waiting for HS binary to halt (mbox0={:#x})\n",
                            gsp_falcon.read_mailbox0()
                        );
                    })?;
                if mbox0 == FLCN_ERR_BINARY_NOT_STARTED {
                    dev_err!(self.dev, "HS binary did not start\n");
                } else {
                    dev_dbg!(self.dev, "HS binary halted with mbox0={:#x}\n", mbox0);
                }

                Ok(())
            },
        )
    }
}

/// Reads the parameter block of type `T` from the start of an event payload, which the ring may
/// have split in two.
///
/// # Errors
///
/// - `EINVAL` if the payload is shorter than `T`.
fn read_params<T: FromBytes + AsBytes + Zeroable>(payload: (&[u8], &[u8])) -> Result<T> {
    let mut params = T::zeroed();

    SBufferIter::new_reader([payload.0, payload.1]).read_exact(params.as_bytes_mut())?;

    Ok(params)
}

/// Payload of a `GMCAPI_CMD_EXEC_HS_BINARY` event.
///
/// GSP-RM has placed the binary in the framebuffer before it sends the event.
///
/// The layout is OpenRM's `GspLoadExecHsBinaryParams`.
#[repr(C)]
#[derive(Debug, Copy, Clone, Zeroable)]
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

impl HsBinaryParams {
    /// FBIF aperture through which the binary is loaded.
    const APERTURE: Bounded<u32, 3> = Bounded::<u32, 3>::new::<0>();
}

// SAFETY: This struct only contains integer types for which all bit patterns are valid.
unsafe impl FromBytes for HsBinaryParams {}

// SAFETY: This struct only contains integer types, laid out without padding.
unsafe impl AsBytes for HsBinaryParams {}

impl<'gsp> super::Gsp<'gsp> {
    /// Attempt to boot the GSP.
    ///
    /// This is a GPU-dependent and complex procedure that involves loading firmware files from
    /// user-space, patching them with signatures, and building firmware-specific intricate data
    /// structures that the GSP will use at runtime.
    ///
    /// Upon return, the GSP is up and running, and its unload bundle (to be given as argument to
    /// [`Self::unload`]) returned.
    pub(crate) fn boot(
        self: Pin<&mut Self>,
        mut ctx: super::GspBootContext<'_, 'gsp>,
    ) -> Result<Option<super::UnloadBundle<'gsp>>> {
        let pdev = ctx.pdev;
        let chipset = ctx.chipset;
        let gsp_falcon = ctx.gsp_falcon;
        let dev = pdev.as_ref();
        let hal = super::hal::gsp_hal(chipset);

        let gsp_fw = KBox::pin_init(GspFirmware::new(dev, chipset), GFP_KERNEL)?;

        self.cmdq
            .send_rpc_command_no_wait(commands::rpc::SetSystemInfo::new(
                pdev,
                chipset,
                ctx.vgpu.state(),
            ))?;
        self.cmdq
            .send_rpc_command_no_wait(commands::rpc::SetRegistry::new(ctx.vgpu.state())?)?;

        // Perform the chipset-specific boot sequence, and retrieve the unload bundle.
        let unload_bundle = hal.boot(&self, &mut ctx, &gsp_fw)?.or_else(|| {
            dev_warn!(dev, "The GSP won't be able to unload properly on unbind.\n");
            dev_warn!(
                dev,
                "The GPU will need to be reset before the driver can bind again.\n"
            );

            None
        });

        let mut unload_guard =
            ScopeGuard::new_with_data((ctx, unload_bundle), |(ctx, unload_bundle)| {
                let _ = self.unload(ctx, unload_bundle);
            });
        let ctx = &mut unload_guard.0;

        gsp_falcon.write_os_version(gsp_fw.bootloader.app_version);

        // Poll for RISC-V to become active before continuing.
        read_poll_timeout(
            || Ok(gsp_falcon.is_riscv_active()),
            |val: &bool| *val,
            Delta::from_millis(10),
            Delta::from_secs(5),
        )?;

        dev_dbg!(pdev, "RISC-V active? {}\n", gsp_falcon.is_riscv_active(),);

        hal.post_boot(&self, ctx, &gsp_fw)?;

        // Wait until GSP is fully initialized.
        commands::rpc::wait_gsp_init_done(&self.cmdq)?;

        Ok(unload_guard.dismiss().1)
    }

    /// Shut down the GSP and wait until it is offline.
    fn shutdown_gsp(
        cmdq: &Cmdq<'_>,
        gsp_falcon: &Falcon<'_, Gsp>,
        mode: commands::rpc::PowerStateLevel,
    ) -> Result {
        // Command to shut the GSP down.
        cmdq.send_rpc_command(commands::rpc::UnloadingGuestDriver::new(mode))?;

        // Wait until GSP signals it is suspended.
        read_poll_timeout(
            || Ok(gsp_falcon.is_processor_suspended()),
            |suspended| *suspended,
            Delta::from_millis(10),
            Delta::from_secs(5),
        )
        .map(|_| ())
    }

    /// Attempts to unload the GSP firmware.
    ///
    /// This stops all activity on the GSP.
    pub(crate) fn unload(
        &self,
        mut ctx: super::GspBootContext<'_, '_>,
        unload_bundle: Option<super::UnloadBundle<'_>>,
    ) -> Result {
        let dev = ctx.dev();

        // Shut down the GSP. Keep going even in case of error.
        let mut res = Self::shutdown_gsp(
            &self.cmdq,
            ctx.gsp_falcon,
            commands::rpc::PowerStateLevel::Level0,
        )
        .inspect_err(|e| dev_err!(dev, "GSP shutdown failed: {:?}\n", e));

        // Run the unload bundle to reset the GSP so it can be booted again.
        if let Some(unload_bundle) = unload_bundle {
            res = res.and(
                unload_bundle
                    .0
                    .run(&mut ctx)
                    .inspect_err(|e| dev_err!(dev, "Unload bundle failed: {:?}\n", e)),
            );
        } else {
            dev_warn!(
                dev,
                "Unload bundle is missing, GSP won't be properly reset.\n"
            );

            res = Err(EAGAIN);
        }

        res.inspect(|()| dev_info!(dev, "GSP successfully unloaded\n"))
    }
}
