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
    transmute::FromBytes,
    types::ScopeGuard, //
};

use crate::{
    falcon::{
        self,
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
    firmware::{
        bindata::request_ucodes_firmware,
        gen_bootloader::{
            BootloaderDmemDescV2,
            GenericBootloader, //
        },
        gsp::GspFirmware,
        radix3::Radix3, //
    },
    gsp::{
        cmdq::Cmdq,
        commands,
        fw::{
            BindataArgs,
            GspArgumentsPadded,
            GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER,
            GMCAPI_CMD_EXEC_HS_BINARY, //
        }, //
    },
    num,
    regs, //
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
    /// DMA address of the LibOS init arguments.
    libos_dma_handle: u64,
}

impl<'gsp> super::Gsp<'gsp> {
    /// Attempt to boot the GSP.
    ///
    /// This is a GPU-dependent and complex procedure that involves loading firmware files from
    /// user-space, patching them with signatures, and building firmware-specific intricate data
    /// structures that the GSP will use at runtime.
    ///
    /// On return, the GSP is running, and the caller gets the static configuration it reported
    /// and the unload bundle for [`Self::unload`].
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

        let gsp_fw = KBox::pin_init(GspFirmware::new(dev, chipset), GFP_KERNEL)?;

        // GSP-RM reads the ucodes image through this page table only while it starts up, so the
        // table is freed when the boot sequence returns.
        let ucodes = request_ucodes_firmware(dev, chipset)?;
        let ucodes_size = ucodes.len();
        let ucodes_radix3 = KBox::pin_init(Radix3::new(dev, ucodes), GFP_KERNEL)?;
        GspArgumentsPadded::set_bindata(
            &self.rmargs,
            &BindataArgs {
                radix3: ucodes_radix3.dma_address(),
                size: num::usize_as_u64(ucodes_size),
            },
        );

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
        let bootloader = if super::hal::uses_generic_bootloader(chipset) {
            Some(GenericBootloader::new(dev, chipset, gsp_falcon)?)
        } else {
            None
        };
        let load_exec = LoadExecContext {
            bootloader: bootloader.as_ref(),
            gsp_falcon,
            sec2_falcon: ctx.sec2_falcon,
            dev,
            bootloader_app_version: gsp_fw.bootloader.app_version,
            libos_dma_handle: self.libos.dma_address(),
        };

        let static_info = commands::gsp_init(&self.cmdq, &init_payload, |command_id, payload| {
            Self::dispatch_gmc_boot_event(&load_exec, command_id, payload)
        })?;

        Ok(super::BootResult {
            unload_bundle: unload_guard.dismiss().1,
            static_info,
        })
    }

    /// Restarts GSP-RM after a load-and-execute image has run.
    ///
    /// # Errors
    ///
    /// - `EIO` if SEC2 reports a failure, or if the GSP is not running RISC-V afterwards.
    /// - `ETIMEDOUT` if SEC2 does not complete the reload in time.
    fn core_resume(ctx: &LoadExecContext<'_, '_>) -> Result {
        let LoadExecContext {
            gsp_falcon,
            sec2_falcon,
            dev,
            ..
        } = *ctx;

        gsp_falcon.reset()?;

        gsp_falcon.write_mailboxes(
            Some(ctx.libos_dma_handle as u32),
            Some((ctx.libos_dma_handle >> 32) as u32),
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

        gsp_falcon.write_os_version(ctx.bootloader_app_version);

        if !gsp_falcon.is_riscv_active() {
            dev_err!(dev, "GSP RISC-V not active after core resume\n");
            return Err(EIO);
        }

        Ok(())
    }

    /// Runs the load-and-execute handler that `command_id` names.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if `command_id` is not a load-and-execute command.
    ///
    /// Errors from the handlers are propagated as-is.
    fn dispatch_gmc_boot_event(
        ctx: &LoadExecContext<'_, '_>,
        command_id: u32,
        payload: &[u8],
    ) -> Result {
        match command_id {
            GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER => Self::handle_load_exec_bootloader(ctx, payload),
            GMCAPI_CMD_EXEC_HS_BINARY => Self::handle_load_exec_hs_binary(ctx, payload),
            _ => {
                dev_err!(
                    ctx.dev,
                    "Unexpected GMC boot event: command_id={:#010x}\n",
                    command_id
                );
                Err(EINVAL)
            }
        }
    }

    /// Handles a `GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER` event: runs the generic bootloader on the
    /// GSP falcon against the descriptor the event carries, then restarts GSP-RM.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if this chipset boots without the generic bootloader, if the payload is shorter
    ///   than the parameter block, if the descriptor is not the size this driver mirrors, or if
    ///   the event names a context DMA slot or an aperture that does not exist.
    /// - `ETIMEDOUT` if the GSP does not suspend, or the image does not halt, in time.
    fn handle_load_exec_bootloader(ctx: &LoadExecContext<'_, '_>, payload: &[u8]) -> Result {
        let LoadExecContext {
            gsp_falcon, dev, ..
        } = *ctx;
        let Some(bootloader) = ctx.bootloader else {
            dev_err!(
                dev,
                "GSP asked for the generic bootloader, which this chipset does not use\n"
            );
            return Err(EINVAL);
        };
        let params = LoadExecGenericBootloaderParams::from_bytes_prefix(payload)
            .ok_or(EINVAL)?
            .0;

        let desc_size =
            u32::try_from(core::mem::size_of::<BootloaderDmemDescV2>()).map_err(|_| EOVERFLOW)?;
        if params.dmem_desc_size != desc_size {
            dev_err!(
                dev,
                "Load-exec descriptor is {} bytes, expected {}\n",
                params.dmem_desc_size,
                desc_size
            );
            return Err(EINVAL);
        }

        let ctx_dma = params.ctx_dma()?;
        let fbif_target = params.fbif_target()?;
        let transcfg =
            || regs::NV_PFALCON_FBIF_TRANSCFG::try_at(usize::from(ctx_dma)).ok_or(EINVAL);

        gsp_falcon.wait_for_processor_suspend().inspect_err(|_| {
            dev_err!(
                dev,
                "Timeout waiting for GSP suspend (mbox0={:#x})\n",
                gsp_falcon.read_mailbox0()
            );
        })?;

        gsp_falcon.reset()?;
        gsp_falcon.dma_reset();

        let saved_transcfg = gsp_falcon.pfalcon.read(transcfg()?);
        gsp_falcon.pfalcon.update(transcfg()?, |v| {
            v.with_target(fbif_target)
                .with_mem_type(FalconFbifMemType::Physical)
        });

        gsp_falcon.pio_load(&bootloader.with_descriptor(&params.dmem_desc))?;

        gsp_falcon.write_mailboxes(Some(FLCN_ERR_BINARY_NOT_STARTED), None);

        gsp_falcon.start()?;
        gsp_falcon.wait_till_halted().inspect_err(|_| {
            dev_err!(
                dev,
                "Timeout waiting for the loaded image to halt (mbox0={:#x})\n",
                gsp_falcon.read_mailbox0()
            );
        })?;

        // A falcon that never halted may still be reading through this aperture, so it is
        // restored only once the image has halted.
        gsp_falcon.pfalcon.update(transcfg()?, |_| saved_transcfg);

        Self::core_resume(ctx)
    }

    /// Handles a `GMCAPI_CMD_EXEC_HS_BINARY` event: runs the high-security binary that GSP-RM
    /// placed in the framebuffer on the GSP falcon, then restarts GSP-RM. The falcon's boot ROM
    /// verifies the binary's signature before it runs.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the payload is shorter than the parameter block, or the ucode id does not
    ///   fit the BROM register field.
    /// - `ETIMEDOUT` if the GSP does not suspend, or the binary does not halt, in time.
    fn handle_load_exec_hs_binary(ctx: &LoadExecContext<'_, '_>, payload: &[u8]) -> Result {
        let LoadExecContext {
            gsp_falcon, dev, ..
        } = *ctx;
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
        gsp_falcon.pfalcon.update(
            regs::NV_PFALCON_FBIF_TRANSCFG::at(usize::from(HS_BINARY_CTX_DMA)),
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

        gsp_falcon.pfalcon2.write(
            Array::at(0),
            regs::NV_PFALCON2_FALCON_BROM_PARAADDR::zeroed().with_value(params.hs_sig_dmem_addr),
        );
        gsp_falcon.pfalcon2.write_reg(
            regs::NV_PFALCON2_FALCON_BROM_ENGIDMASK::zeroed().with_value(params.engine_id_mask),
        );
        gsp_falcon.pfalcon2.write_reg(
            regs::NV_PFALCON2_FALCON_BROM_CURR_UCODE_ID::zeroed()
                .with_ucode_id(u8::try_from(params.ucode_id).map_err(|_| EINVAL)?),
        );
        gsp_falcon.pfalcon2.write_reg(
            regs::NV_PFALCON2_FALCON_MOD_SEL::zeroed().with_algo(FalconModSelAlgo::Rsa3k),
        );

        gsp_falcon.write_mailboxes(Some(FLCN_ERR_BINARY_NOT_STARTED), None);

        gsp_falcon
            .pfalcon
            .write_reg(regs::NV_PFALCON_FALCON_BOOTVEC::zeroed().with_value(params.ucode_imem_va));

        gsp_falcon.start()?;
        gsp_falcon.wait_till_halted().inspect_err(|_| {
            dev_err!(
                dev,
                "Timeout waiting for HS binary to halt (mbox0={:#x})\n",
                gsp_falcon.read_mailbox0()
            );
        })?;

        Self::core_resume(ctx)
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

/// `MAILBOX0` marker meaning the falcon binary has not started. A binary that runs replaces it
/// with its own status, and the write also clears the suspend bit for the next event's wait.
const FLCN_ERR_BINARY_NOT_STARTED: u32 = 0xfe;

/// `ucode_dmem_va` value meaning the binary has no DMEM virtual address.
const FLCN_DMEM_VA_INVALID: u32 = 0xffff_ffff;

/// Context DMA slot the HS binary is loaded through.
const HS_BINARY_CTX_DMA: u8 = 0;

/// Payload of a `GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER` event.
///
/// The descriptor carries the code and data addresses, and `addr_space` and `cpu_cache_attrib`
/// say which FBIF aperture reaches them.
#[repr(C)]
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

    /// Returns the context DMA slot for fetching the image.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the slot is outside the FBIF `TRANSCFG` array.
    fn ctx_dma(&self) -> Result<u8> {
        let ctx_dma = self.dmem_desc.ctx_dma;

        u8::try_from(ctx_dma)
            .ok()
            .filter(|slot| *slot < falcon::NUM_CTX_DMA_SLOTS)
            .ok_or(EINVAL)
    }

    /// Returns the FBIF aperture that reaches the image.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the address space and cache attribute pair is not one this driver maps.
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

/// Payload of a `GMCAPI_CMD_EXEC_HS_BINARY` event.
///
/// GSP-RM has written the code to `imem_phys_addr` and the data to `dmem_phys_addr` in the
/// framebuffer before it sends the event.
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
