// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use kernel::{
    io::poll::read_poll_timeout,
    prelude::*,
    time::Delta,
    types::ScopeGuard, //
};

use crate::{
    falcon::{
        gsp::Gsp,
        Falcon, //
    },
    firmware::{
        bindata::UcodesImage,
        gsp::GspFirmware, //
    },
    gsp::{
        cmdq::Cmdq,
        commands::gmc::{
            GspInit,
            GspSuspend,
            PowerStateLevel, //
        },
        fw::GspArgumentsPadded,
        load_exec, //
    },
};

impl<'gsp> super::Gsp<'gsp> {
    /// Attempt to boot the GSP.
    ///
    /// This is a GPU-dependent and complex procedure that involves loading firmware files from
    /// user-space, patching them with signatures, and building firmware-specific intricate data
    /// structures that the GSP will use at runtime.
    ///
    /// Returns, with the GSP running, the static configuration that GSP-RM reported and the
    /// unload bundle for [`Self::unload`].
    ///
    /// # Errors
    ///
    /// - `ENOENT` if the ucodes image is not installed.
    /// - `EIO` if GSP-RM answers `GSP_INIT` with an error status.
    pub(crate) fn boot(
        self: Pin<&mut Self>,
        mut ctx: super::GspBootContext<'_, 'gsp>,
    ) -> Result<super::BootResult<'gsp>> {
        let pdev = ctx.pdev;
        let chipset = ctx.chipset;
        let gsp_falcon = ctx.gsp_falcon;
        let dev = pdev.as_ref();
        let hal = super::hal::gsp_hal(chipset);

        let gsp_fw = KBox::pin_init(GspFirmware::new(dev, chipset), GFP_KERNEL)?;

        let generic_bootloader = hal.generic_bootloader(dev, chipset)?;

        // GSP-RM reads the ucodes image through the image's page table only while it starts up, so
        // the image is freed when the boot sequence returns.
        let ucodes = UcodesImage::new(dev, chipset)?;
        GspArgumentsPadded::set_bindata(&self.rmargs, &ucodes);

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

        // GSP-RM answers GSP_INIT once it has finished starting, which includes the
        // load-and-execute events that it raises along the way.
        let libos_dma_address = self.libos.dma_address();
        let bootloader_app_version = gsp_fw.bootloader.app_version;
        let static_info = self
            .cmdq
            .send_gmc_command(GspInit::new(pdev, chipset, ctx.vgpu.state())?, |event| {
                load_exec::handle_event(
                    ctx,
                    generic_bootloader.as_ref(),
                    libos_dma_address,
                    bootloader_app_version,
                    event,
                )
            })?
            .map_err(|status| {
                dev_err!(dev, "GSP_INIT failed with status {:#x}\n", status);
                EIO
            })?;

        Ok(super::BootResult {
            unload_bundle: unload_guard.dismiss().1,
            static_info,
        })
    }

    /// Shut down the GSP and wait until it is offline.
    fn shutdown_gsp(
        cmdq: &Cmdq<'_>,
        gsp_falcon: &Falcon<'_, Gsp>,
        mode: PowerStateLevel,
    ) -> Result {
        // Command to shut the GSP down. GSP-RM does not answer it.
        cmdq.send_gmc_command_no_wait(GspSuspend::new(mode))?;

        // Wait until GSP signals it is suspended. GSP-RM posts messages while it suspends, and the
        // GSP event interrupt is already freed, so this poll drains them.
        read_poll_timeout(
            || {
                cmdq.drain()?;

                Ok(gsp_falcon.is_processor_suspended())
            },
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
        let mut res = Self::shutdown_gsp(&self.cmdq, ctx.gsp_falcon, PowerStateLevel::Level0)
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
