// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use kernel::{
    device,
    io::{
        poll::read_poll_timeout,
        register::WithBase,
        Io, //
    },
    prelude::*,
    time::Delta,
    transmute::FromBytes,
    types::ScopeGuard, //
};

use crate::{
    driver::Bar0,
    falcon::{
        gsp::Gsp,
        sec2::Sec2,
        Falcon,
        FalconDmaSrcOffset,
        FalconFbifEngineIdFlag,
        FalconFbifMemType,
        FalconFbifTarget,
        FalconMem,
        FalconModSelAlgo, //
    },
    firmware::gsp::GspFirmware,
    gsp::{
        cmdq::Cmdq,
        commands, //
    },
    regs, //
};

impl super::Gsp {
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
        mut ctx: super::GspBootContext<'_, '_>,
    ) -> Result<Option<super::UnloadBundle>> {
        let pdev = ctx.pdev;
        let bar = ctx.bar;
        let chipset = ctx.chipset;
        let gsp_falcon = ctx.gsp_falcon;
        let dev = pdev.as_ref();
        let hal = super::hal::gsp_hal(chipset);

        let gsp_fw = KBox::pin_init(GspFirmware::new(dev, chipset, &self.gsp_tlv), GFP_KERNEL)?;

        self.cmdq
            .send_command_no_wait(bar, commands::SetSystemInfo::new(pdev, chipset))?;
        self.cmdq
            .send_command_no_wait(bar, commands::SetRegistry::new(ctx.vgpu.state())?)?;

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
        commands::wait_gsp_init_done(&self.cmdq)?;

        Ok(unload_guard.dismiss().1)
    }

    /// Restart GSP-RM once a load-and-execute image has run to completion.
    ///
    /// Resets the GSP falcon into RISC-V, hands it the libos boot arguments address through its
    /// mailboxes, and starts SEC2, which is what brings GSP-RM back up. Open RM calls this
    /// `kgspExecuteCoreResume`.
    ///
    /// # Errors
    ///
    /// - `EIO` if SEC2 reports a failure, or if the GSP is not running RISC-V afterwards.
    /// - `ETIMEDOUT` if SEC2 does not complete the reload in time.
    fn core_resume(
        gsp_falcon: &Falcon<'_, Gsp>,
        sec2_falcon: &Falcon<'_, Sec2>,
        dev: &device::Device,
        bootloader_app_version: u32,
        libos_dma_handle: u64,
    ) -> Result {
        gsp_falcon.reset()?;

        gsp_falcon.write_mailboxes(
            Some(libos_dma_handle as u32),
            Some((libos_dma_handle >> 32) as u32),
        );

        sec2_falcon.start()?;

        gsp_falcon
            .check_reload_completed(Delta::from_secs(2))
            .inspect_err(|_| {
                let mbox0 = sec2_falcon.read_mailbox0();
                dev_err!(
                    dev,
                    "Timeout waiting for SEC2 to resume GSP-RM (SEC2 mbox0={:#x})\n",
                    mbox0
                );
            })?;

        let sec2_mbox0 = sec2_falcon.read_mailbox0();
        if sec2_mbox0 != 0 {
            dev_err!(
                dev,
                "SEC2 reported error during core resume: {:#x}\n",
                sec2_mbox0
            );
            return Err(EIO);
        }

        gsp_falcon.write_os_version(bootloader_app_version);

        if !gsp_falcon.is_riscv_active() {
            dev_err!(dev, "GSP RISC-V not active after core resume\n");
            return Err(EIO);
        }

        Ok(())
    }

    /// Handle a `GMCAPI_CMD_EXEC_HS_BINARY` event.
    ///
    /// GSP-RM has already placed a high-security binary in the framebuffer. DMA it into falcon
    /// memory, program the BROM registers that make the falcon verify its PKC signature, run it,
    /// and resume GSP-RM.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the payload is shorter than the parameter block, or the ucode id does not
    ///   fit the BROM register field.
    /// - `ETIMEDOUT` if the GSP does not suspend, or the binary does not halt, in time.
    #[expect(dead_code)]
    fn handle_load_exec_hs_binary(
        payload: &[u8],
        gsp_falcon: &Falcon<'_, Gsp>,
        sec2_falcon: &Falcon<'_, Sec2>,
        bar: Bar0<'_>,
        dev: &device::Device,
        bootloader_app_version: u32,
        libos_dma_handle: u64,
    ) -> Result {
        let params = HsBinaryParams::from_bytes_prefix(payload).ok_or(EINVAL)?.0;

        gsp_falcon.wait_for_processor_suspend().inspect_err(|_| {
            dev_err!(
                dev,
                "Timeout waiting for GSP suspend (mbox0={:#x})\n",
                gsp_falcon.read_mailbox0()
            );
        })?;

        gsp_falcon.reset()?;

        gsp_falcon.dma_reset();
        bar.update(
            regs::NV_PFALCON_FBIF_TRANSCFG::of::<Gsp>().at(usize::from(HS_BINARY_CTX_DMA)),
            |v| {
                v.with_target(FalconFbifTarget::LocalFb)
                    .with_mem_type(FalconFbifMemType::Physical)
                    .with_engine_id_flag(FalconFbifEngineIdFlag::Bar2Fn0)
            },
        );

        if params.ucode_imem_size > 0 {
            gsp_falcon.raw_dma_transfer(
                HS_BINARY_CTX_DMA,
                params.imem_phys_addr,
                FalconMem::ImemSecure,
                FalconDmaSrcOffset::Offset(params.ucode_imem_va),
                params.ucode_imem_pa,
                params.ucode_imem_size,
            )?;
        }

        if params.ucode_dmem_size > 0 {
            // A valid DMEM virtual address makes the engine tag each loaded block with it, which
            // is how the binary reaches its data.
            let src = if params.ucode_dmem_va == FLCN_DMEM_VA_INVALID {
                FalconDmaSrcOffset::Offset(0)
            } else {
                FalconDmaSrcOffset::DmemVa(params.ucode_dmem_va)
            };

            gsp_falcon.raw_dma_transfer(
                HS_BINARY_CTX_DMA,
                params.dmem_phys_addr,
                FalconMem::Dmem,
                src,
                params.ucode_dmem_pa,
                params.ucode_dmem_size,
            )?;
        }

        bar.write(
            WithBase::of::<Gsp>().at(0),
            regs::NV_PFALCON2_FALCON_BROM_PARAADDR::zeroed().with_value(params.hs_sig_dmem_addr),
        );
        bar.write(
            WithBase::of::<Gsp>(),
            regs::NV_PFALCON2_FALCON_BROM_ENGIDMASK::zeroed().with_value(params.engine_id_mask),
        );
        bar.write(
            WithBase::of::<Gsp>(),
            regs::NV_PFALCON2_FALCON_BROM_CURR_UCODE_ID::zeroed()
                .with_ucode_id(u8::try_from(params.ucode_id).map_err(|_| EINVAL)?),
        );
        bar.write(
            WithBase::of::<Gsp>(),
            regs::NV_PFALCON2_FALCON_MOD_SEL::zeroed().with_algo(FalconModSelAlgo::Rsa3k),
        );

        gsp_falcon.write_mailboxes(Some(FLCN_ERR_BINARY_NOT_STARTED), None);

        bar.write(
            WithBase::of::<Gsp>(),
            regs::NV_PFALCON_FALCON_BOOTVEC::zeroed().with_value(params.ucode_imem_va),
        );

        gsp_falcon.start()?;
        gsp_falcon.wait_till_halted().inspect_err(|_| {
            dev_err!(
                dev,
                "Timeout waiting for HS binary to halt (mbox0={:#x})\n",
                gsp_falcon.read_mailbox0()
            );
        })?;

        Self::core_resume(
            gsp_falcon,
            sec2_falcon,
            dev,
            bootloader_app_version,
            libos_dma_handle,
        )
    }

    /// Shut down the GSP and wait until it is offline.
    fn shutdown_gsp(
        cmdq: &Cmdq,
        bar: Bar0<'_>,
        gsp_falcon: &Falcon<'_, Gsp>,
        mode: commands::PowerStateLevel,
    ) -> Result {
        // Command to shut the GSP down.
        cmdq.send_command(bar, commands::UnloadingGuestDriver::new(mode))?;

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
        unload_bundle: Option<super::UnloadBundle>,
    ) -> Result {
        let dev = ctx.dev();

        // Shut down the GSP. Keep going even in case of error.
        let mut res = Self::shutdown_gsp(
            &self.cmdq,
            ctx.bar,
            ctx.gsp_falcon,
            commands::PowerStateLevel::Level0,
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

/// Value Open RM leaves in `MAILBOX0` before starting a falcon binary, so a binary that never
/// runs is distinguishable from one that ran and returned success. The write also clears the
/// suspend bit, which is what lets the next event wait on a suspend of its own.
const FLCN_ERR_BINARY_NOT_STARTED: u32 = 0xfe;

/// `ucode_dmem_va` value meaning the binary has no DMEM virtual address.
const FLCN_DMEM_VA_INVALID: u32 = 0xffff_ffff;

/// Context DMA slot the HS binary is loaded through. Open RM hardcodes slot 0 for this event and
/// points it at local framebuffer.
const HS_BINARY_CTX_DMA: u8 = 0;

/// Parameters for loading and executing an HS (high-security) binary.
///
/// GSP-RM sends these as the payload of `GMCAPI_CMD_EXEC_HS_BINARY`, having already written the
/// code to `imem_phys_addr` and the data to `dmem_phys_addr` in the framebuffer.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct HsBinaryParams {
    imem_phys_addr: u64,
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
