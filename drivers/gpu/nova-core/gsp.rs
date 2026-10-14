// SPDX-License-Identifier: GPL-2.0

mod boot;
mod hal;

use kernel::{
    debugfs,
    device,
    dma::{
        Coherent,
        CoherentBox,
        CoherentView,
        DmaAddress, //
    },
    firmware,
    io::io_write,
    pci,
    prelude::*, //
};

pub(crate) mod cmdq;
pub(crate) mod commands;
mod fw;
mod logbuffer;
mod nvkv;
mod regs;

use logbuffer::LogBuffers;
pub(crate) use logbuffer::RetainedLogs;

pub(crate) use fw::{
    GspFmcBootParams,
    GspFwWprMeta,
    LibosMemoryRegionInitArgument,
    LibosParams, //
};
pub(crate) use hal::boot_firmware_files;

use crate::{
    driver::Bar0,
    falcon::{
        gsp::Gsp as GspFalcon,
        sec2::Sec2 as Sec2Falcon,
        Falcon, //
    },
    firmware::{
        gsp::BuildId,
        tlv::{
            request_tlv,
            Tlv, //
        },
    },
    fsp::Fsp,
    gpu::{
        Chipset,
        Spec, //
    },
    gsp::{
        cmdq::Cmdq,
        fw::GspArgumentsPadded, //
    },
    num,
    vgpu::VgpuManager, //
};

pub(crate) const GSP_PAGE_SHIFT: usize = 12;
pub(crate) const GSP_PAGE_SIZE: usize = 1 << GSP_PAGE_SHIFT;

/// Common context for the GSP boot process.
///
/// It carries two distinct lifetimes:
///
/// - `'gpu` is the lifetime of the bound GPU device, as captured by the GPU subdevices.
/// - `'ctx` is a shorter lifetime during which this context borrows those subdevices.
pub(crate) struct GspBootContext<'ctx, 'gpu> {
    pub(crate) pdev: &'gpu pci::Device<device::Bound>,
    pub(crate) bar: Bar0<'gpu>,
    pub(crate) chipset: Chipset,
    pub(crate) gsp_falcon: &'ctx Falcon<'gpu, GspFalcon>,
    pub(crate) sec2_falcon: &'ctx Falcon<'gpu, Sec2Falcon>,
    pub(crate) fsp: Option<&'ctx mut Fsp<'gpu>>,
    pub(crate) vgpu: &'ctx VgpuManager,
}

impl<'ctx, 'gpu> GspBootContext<'ctx, 'gpu> {
    pub(crate) fn dev(&self) -> &'gpu device::Device<device::Bound> {
        self.pdev.as_ref()
    }
}

/// Array of page table entries, as understood by the GSP bootloader.
#[repr(C)]
#[derive(FromBytes, IntoBytes)]
struct PteArray<const NUM_ENTRIES: usize>([u64; NUM_ENTRIES]);

impl<const NUM_PAGES: usize> PteArray<NUM_PAGES> {
    /// Initialize a new page table array mapping `NUM_PAGES` GSP pages starting at address `start`.
    fn init(view: CoherentView<'_, Self>, start: DmaAddress) -> Result<()> {
        for i in 0..NUM_PAGES {
            io_write!(view, .0[build: i],
                start
                    .checked_add(num::usize_as_u64(i) << GSP_PAGE_SHIFT)
                    .ok_or(EOVERFLOW)?
            );
        }

        Ok(())
    }
}

/// GSP runtime data.
#[pin_data]
pub(crate) struct Gsp<'gsp> {
    /// The GSP firmware's TLV.
    gsp_tlv: firmware::Firmware,
    /// Libos arguments.
    pub(crate) libos: Coherent<'gsp, [LibosMemoryRegionInitArgument]>,
    /// Log buffers, optionally exposed via debugfs.
    #[pin]
    logs: debugfs::Scope<LogBuffers<'gsp>>,
    /// Command queue, borrowed by the GSP event interrupt handler.
    #[pin]
    pub(crate) cmdq: Cmdq<'gsp>,
    /// RM arguments.
    rmargs: Coherent<'gsp, GspArgumentsPadded>,
    /// Buffer in which GSP-RM reports its own state.
    rm_state_monitor: Coherent<'gsp, [u8; GSP_PAGE_SIZE]>,
}

impl<'gsp> Gsp<'gsp> {
    // Creates an in-place initializer for a `Gsp` manager for `pdev`.
    pub(crate) fn new(
        pdev: &'gsp pci::Device<device::Bound>,
        spec: Spec,
        bar: Bar0<'gsp>,
    ) -> impl PinInit<Self, Error> + 'gsp {
        pin_init::pin_init_scope(move || {
            let dev = pdev.as_ref();

            let gsp_tlv = request_tlv(dev, spec.chipset, "gsp")?;
            let build_id = BuildId::from_tlv(&Tlv::new(gsp_tlv.data())?)
                .inspect_err(|_| {
                    dev_warn!(
                        dev,
                        "no build ID in the GSP firmware TLV, so its log dumps carry no header\n"
                    )
                })
                .ok();
            let log_buffers = LogBuffers::new(dev, spec, build_id.as_ref())?;

            Ok(try_pin_init!(Self {
                gsp_tlv,
                cmdq <- Cmdq::new(dev, bar),
                rm_state_monitor: Coherent::zeroed(dev, GFP_KERNEL)?,
                rmargs: Coherent::init(
                    dev,
                    GFP_KERNEL,
                    GspArgumentsPadded::new(&cmdq, rm_state_monitor),
                )?,
                libos: {
                    let mut libos = CoherentBox::zeroed_slice(
                        dev,
                        GSP_PAGE_SIZE / size_of::<LibosMemoryRegionInitArgument>(),
                        GFP_KERNEL,
                    )?;

                    log_buffers.init_arguments(&mut libos)?;
                    libos.init_at(
                        LogBuffers::COUNT,
                        LibosMemoryRegionInitArgument::new("RMARGS", rmargs),
                    )?;

                    libos.into()
                },
                logs <- log_buffers.scope(),
            }))
        })
    }
}

/// Opaque bundle required to unload the GSP. Created by [`Gsp::boot`], consumed by [`Gsp::unload`].
pub(crate) struct UnloadBundle<'a>(KBox<dyn hal::UnloadBundle + 'a>);

/// The results of [`Gsp::boot`]: the static GPU configuration and the unload bundle.
pub(crate) struct BootResult<'a> {
    /// The unload bundle for [`Gsp::unload`], if one could be built.
    pub(crate) unload_bundle: Option<UnloadBundle<'a>>,
    /// The static GPU configuration, as decoded from the `GSP_INIT` reply.
    pub(crate) static_info: commands::GspStaticInfo,
}
