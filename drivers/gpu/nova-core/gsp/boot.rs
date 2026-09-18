// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use kernel::{
    device,
    io::{
        poll::read_poll_timeout,
        register::Array,
        Io, //
    },
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
        sec2::Sec2,
        Falcon,
        FalconDmaSrcOffset,
        FalconFbifEngineIdFlag,
        FalconFbifMemType,
        FalconFbifTarget,
        FalconMem,
        FalconModSelAlgo,
        FLCN_ERR_BINARY_NOT_STARTED, //
    },
    firmware::{
        bindata::UcodesImage,
        gen_bootloader::{
            BootloaderDmemDescV2,
            GenericBootloader, //
        },
        gsp::GspFirmware, //
    },
    gsp::{
        cmdq::Cmdq,
        commands,
        fw::{
            GspArgumentsPadded,
            GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER,
            GMCAPI_CMD_EXEC_HS_BINARY, //
        }, //
    },
    regs,
    sbuffer::SBufferIter, //
};

/// The falcons, device and boot parameters that the load-and-execute event handlers share.
struct LoadExecContext<'a, 'gpu> {
    /// The generic falcon bootloader, on chipsets that boot through it.
    bootloader: Option<&'a GenericBootloader>,
    gsp_falcon: &'a Falcon<'gpu, Gsp>,
    sec2_falcon: &'a Falcon<'gpu, Sec2>,
    dev: &'a device::Device,
    /// GSP bootloader application version.
    bootloader_app_version: u32,
    /// DMA address of the LIBOS init arguments.
    libos_dma_handle: u64,
}

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
        let Self {
            gsp_falcon, dev, ..
        } = *self;

        gsp_falcon.wait_for_processor_suspend().inspect_err(|_| {
            dev_err!(
                dev,
                "Timeout waiting for GSP suspend (mbox0={:#x})\n",
                gsp_falcon.read_mailbox0()
            );
        })?;

        gsp_falcon.reset()?;
        gsp_falcon.dma_reset();

        Ok(())
    }

    /// Runs the core resume, in which SEC2 restarts GSP-RM after a load-and-execute binary has
    /// halted on the GSP falcon.
    ///
    /// # Errors
    ///
    /// - `EIO` if SEC2 reports a failure, or if the GSP is not running RISC-V afterwards.
    /// - `ETIMEDOUT` if SEC2 does not complete the reload within two seconds.
    fn core_resume(&self) -> Result {
        let Self {
            gsp_falcon,
            sec2_falcon,
            dev,
            ..
        } = *self;

        gsp_falcon.reset()?;

        gsp_falcon.write_mailboxes(
            Some(self.libos_dma_handle as u32),
            Some((self.libos_dma_handle >> 32) as u32),
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

        gsp_falcon.write_os_version(self.bootloader_app_version);

        if !gsp_falcon.is_riscv_active() {
            dev_err!(dev, "GSP RISC-V not active after core resume\n");
            return Err(EIO);
        }

        Ok(())
    }

    /// Runs the load-and-execute handler that `command_id` names on the event payload, which the
    /// ring may have split in two, and then runs the core resume.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if `command_id` is not a load-and-execute command.
    ///
    /// Errors from the handlers and from [`Self::core_resume`] are propagated as-is.
    fn dispatch_gmc_boot_event(
        &self,
        command_id: u32,
        payload_0: &[u8],
        payload_1: &[u8],
    ) -> Result {
        let handled = match command_id {
            GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER => {
                self.handle_load_exec_bootloader(payload_0, payload_1)
            }
            GMCAPI_CMD_EXEC_HS_BINARY => self.handle_load_exec_hs_binary(payload_0, payload_1),
            _ => {
                dev_err!(
                    self.dev,
                    "Unexpected GMC boot event: command_id={:#010x}\n",
                    command_id
                );
                return Err(EINVAL);
            }
        };

        handled.and_then(|()| self.core_resume()).inspect_err(|e| {
            dev_err!(
                self.dev,
                "GMC boot event {:#010x} failed: {:?}\n",
                command_id,
                e
            );
        })
    }

    /// Runs the generic bootloader on the GSP falcon, as a `GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER`
    /// event requests.
    ///
    /// The descriptor that the event carries names the image that the bootloader loads. The GSP
    /// falcon is left halted.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if this chipset boots without the generic bootloader, if the payload is shorter
    ///   than the parameter block, if the descriptor is not the size that this driver defines for
    ///   it, or if the event names a context DMA slot or an aperture that does not exist.
    /// - `ETIMEDOUT` if the RISC-V core does not suspend within two seconds, or the GSP falcon does
    ///   not halt within two seconds of starting the image.
    fn handle_load_exec_bootloader(&self, payload_0: &[u8], payload_1: &[u8]) -> Result {
        let Self {
            gsp_falcon, dev, ..
        } = *self;
        let Some(bootloader) = self.bootloader else {
            dev_err!(
                dev,
                "GSP asked for the generic bootloader, which this chipset does not use\n"
            );
            return Err(EINVAL);
        };
        let params = read_params::<LoadExecGenericBootloaderParams>(payload_0, payload_1)?;

        if params.dmem_desc_size != BootloaderDmemDescV2::SIZE {
            dev_err!(
                dev,
                "Load-exec descriptor is {} bytes, expected {}\n",
                params.dmem_desc_size,
                BootloaderDmemDescV2::SIZE
            );
            return Err(EINVAL);
        }

        let fbif_target = params.fbif_target()?;

        self.reset_gsp_falcon_after_suspend()?;

        gsp_falcon.with_fbif_transcfg(
            params.dmem_desc.ctx_dma,
            |v| {
                v.with_target(fbif_target)
                    .with_mem_type(FalconFbifMemType::Physical)
            },
            || {
                gsp_falcon.pio_load(&bootloader.with_descriptor(&params.dmem_desc))?;

                let (mbox0, _) = gsp_falcon
                    .boot(Some(FLCN_ERR_BINARY_NOT_STARTED), None)
                    .inspect_err(|_| {
                        dev_err!(
                            dev,
                            "Timeout waiting for the loaded image to halt (mbox0={:#x})\n",
                            gsp_falcon.read_mailbox0()
                        );
                    })?;
                dev_dbg!(dev, "Loaded image halted with mbox0={:#x}\n", mbox0);

                Ok(())
            },
        )
    }

    /// Runs a Heavy-Secured (HS) binary on the GSP falcon, as a `GMCAPI_CMD_EXEC_HS_BINARY` event
    /// requests.
    ///
    /// GSP-RM has placed the binary in the framebuffer, and the falcon's boot ROM (BROM) verifies
    /// the binary's signature before the binary runs. The GSP falcon is left halted.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the payload is shorter than the parameter block, or the ucode id does not
    ///   fit the BROM register field.
    /// - `ETIMEDOUT` if the RISC-V core does not suspend within two seconds, or the GSP falcon does
    ///   not halt within two seconds of starting the binary.
    fn handle_load_exec_hs_binary(&self, payload_0: &[u8], payload_1: &[u8]) -> Result {
        let Self {
            gsp_falcon, dev, ..
        } = *self;
        let params = read_params::<HsBinaryParams>(payload_0, payload_1)?;

        self.reset_gsp_falcon_after_suspend()?;

        gsp_falcon.with_fbif_transcfg(
            HsBinaryParams::CTX_DMA,
            |v| {
                v.with_target(FalconFbifTarget::LocalFb)
                    .with_mem_type(FalconFbifMemType::Physical)
                    .with_engine_id_flag(FalconFbifEngineIdFlag::Bar2Fn0)
            },
            || {
                if params.ucode_imem_size > 0 {
                    gsp_falcon.raw_dma_transfer(
                        HsBinaryParams::CTX_DMA,
                        params.imem_phys_addr,
                        FalconMem::ImemSecure,
                        FalconDmaSrcOffset::Va(params.ucode_imem_va),
                        params.ucode_imem_pa,
                        params.ucode_imem_size,
                    )?;
                }

                if params.ucode_dmem_size > 0 {
                    gsp_falcon.raw_dma_transfer(
                        HsBinaryParams::CTX_DMA,
                        params.dmem_phys_addr,
                        FalconMem::Dmem,
                        FalconDmaSrcOffset::from_dmem_va(params.ucode_dmem_va),
                        params.ucode_dmem_pa,
                        params.ucode_dmem_size,
                    )?;
                }

                gsp_falcon.pfalcon2.write(
                    Array::at(0),
                    regs::NV_PFALCON2_FALCON_BROM_PARAADDR::zeroed()
                        .with_value(params.hs_sig_dmem_addr),
                );
                gsp_falcon.pfalcon2.write_reg(
                    regs::NV_PFALCON2_FALCON_BROM_ENGIDMASK::zeroed()
                        .with_value(params.engine_id_mask),
                );
                gsp_falcon.pfalcon2.write_reg(
                    regs::NV_PFALCON2_FALCON_BROM_CURR_UCODE_ID::zeroed()
                        .with_ucode_id(u8::try_from(params.ucode_id).map_err(|_| EINVAL)?),
                );
                gsp_falcon.pfalcon2.write_reg(
                    regs::NV_PFALCON2_FALCON_MOD_SEL::zeroed().with_algo(FalconModSelAlgo::Rsa3k),
                );

                gsp_falcon.pfalcon.write_reg(
                    regs::NV_PFALCON_FALCON_BOOTVEC::zeroed().with_value(params.ucode_imem_va),
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
                dev_dbg!(dev, "HS binary halted with mbox0={:#x}\n", mbox0);

                Ok(())
            },
        )
    }
}

impl<'gsp> super::Gsp<'gsp> {
    /// Boots the GSP.
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
    pub(crate) fn boot(
        self: Pin<&mut Self>,
        mut ctx: super::GspBootContext<'_, 'gsp>,
    ) -> Result<super::BootResult<'gsp>> {
        let pdev = ctx.pdev;
        let chipset = ctx.chipset;
        let gsp_falcon = ctx.gsp_falcon;
        let dev = pdev.as_ref();
        let hal = super::hal::gsp_hal(chipset);

        let gsp_fw = KBox::pin_init(GspFirmware::new(dev, chipset, &self.gsp_tlv), GFP_KERNEL)?;

        let generic_bootloader = hal.generic_bootloader(dev, chipset, gsp_falcon.imem_size())?;

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

        let init_payload = commands::build_gsp_init_payload(pdev, chipset, ctx.vgpu.state())?;
        let load_exec = LoadExecContext {
            bootloader: generic_bootloader.as_ref(),
            gsp_falcon,
            sec2_falcon: ctx.sec2_falcon,
            dev,
            bootloader_app_version: gsp_fw.bootloader.app_version,
            libos_dma_handle: self.libos.dma_address(),
        };

        let static_info =
            commands::gsp_init(&self.cmdq, &init_payload, |header, payload_0, payload_1| {
                load_exec.dispatch_gmc_boot_event(header.gmc.command_id(), payload_0, payload_1)
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
        mode: commands::PowerStateLevel,
    ) -> Result {
        commands::gsp_suspend(cmdq, mode)?;

        // GSP-RM posts messages while it suspends, and the GSP event interrupt is already freed,
        // so this poll drains them.
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
        let mut res = Self::shutdown_gsp(
            &self.cmdq,
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

/// Reads the parameter block of type `T` from the start of an event payload, which the ring may
/// have split in two.
///
/// # Errors
///
/// - `EINVAL` if the payload is shorter than `T`.
fn read_params<T: FromBytes + AsBytes + Zeroable>(payload_0: &[u8], payload_1: &[u8]) -> Result<T> {
    let mut params = T::zeroed();

    SBufferIter::new_reader([payload_0, payload_1]).read_exact(params.as_bytes_mut())?;

    Ok(params)
}

/// Payload of a `GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER` event.
///
/// The descriptor carries the code and data addresses, and `addr_space` and `cpu_cache_attrib`
/// say which FBIF (framebuffer interface) aperture reaches them.
#[repr(C)]
#[derive(Zeroable)]
struct LoadExecGenericBootloaderParams {
    dmem_desc: BootloaderDmemDescV2,
    dmem_desc_size: u32,
    addr_space: u32,
    cpu_cache_attrib: u32,
    _reserved: [u32; 4],
}

impl LoadExecGenericBootloaderParams {
    const ADDR_SYSMEM: u32 = 1;
    const ADDR_FBMEM: u32 = 2;
    const NV_MEMORY_CACHED: u32 = 0;
    const NV_MEMORY_UNCACHED: u32 = 1;

    /// Returns the FBIF aperture that reaches the image.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the pair of address space and cache attribute is not one that this driver
    ///   maps.
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
unsafe impl FromBytes for LoadExecGenericBootloaderParams {}

// SAFETY: The nested descriptor is `AsBytes`, every other field is an integer type, and the
// layout has no padding.
unsafe impl AsBytes for LoadExecGenericBootloaderParams {}

/// Payload of a `GMCAPI_CMD_EXEC_HS_BINARY` event.
///
/// GSP-RM has written the code to `imem_phys_addr` and the data to `dmem_phys_addr` in the
/// framebuffer before it sends the event.
#[repr(C)]
#[derive(Debug, Copy, Clone, Zeroable)]
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

impl HsBinaryParams {
    /// Context DMA slot through which the binary is loaded.
    const CTX_DMA: u32 = 0;
}

// SAFETY: This struct only contains integer types for which all bit patterns are valid.
unsafe impl FromBytes for HsBinaryParams {}

// SAFETY: This struct only contains integer types, laid out without padding.
unsafe impl AsBytes for HsBinaryParams {}
