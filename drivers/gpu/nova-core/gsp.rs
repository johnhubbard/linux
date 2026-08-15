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
    io::{
        io_project,
        io_write,
        Io, //
    },
    pci,
    prelude::*, //
};

pub(crate) mod cmdq;
pub(crate) mod commands;
mod fw;
#[cfg_attr(not(CONFIG_KUNIT), allow(dead_code))]
mod nvkv;
mod regs;
mod sequencer;

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
    fsp::Fsp,
    gpu::Chipset,
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

/// Number of GSP pages to use in a RM log buffer.
const RM_LOG_BUFFER_NUM_PAGES: usize = 0x10;

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

/// The logging buffers are byte queues that contain encoded printf-like
/// messages from GSP-RM.  They need to be decoded by a special application
/// that can parse the buffers.
///
/// The 'loginit' buffer contains logs from early GSP-RM init and
/// exception dumps.  The 'logrm' buffer contains the subsequent logs. Both are
/// written to directly by GSP-RM and can be any multiple of GSP_PAGE_SIZE.
///
/// The physical address map for the log buffer is stored in the buffer
/// itself, starting with offset 1. Offset 0 contains the "put" pointer (pp).
/// Initially, pp is equal to 0. If the buffer has valid logging data in it,
/// then pp points to index into the buffer where the next logging entry will
/// be written. Therefore, the logging data is valid if:
///   1 <= pp < sizeof(buffer)/sizeof(u64)
struct LogBuffer<const NUM_PAGES: usize>(Coherent<[[u8; GSP_PAGE_SIZE]; NUM_PAGES]>);

/// A log buffer at the default size, [`RM_LOG_BUFFER_NUM_PAGES`] pages.
///
/// Matches the registry defaults for the init, interrupt, RM and MNOC tasks
/// (`NV_REG_STR_RM_GSP_LOG_BUFFER_SIZE_TASK_*_DEFAULT`).
type TaskLogBuffer = LogBuffer<RM_LOG_BUFFER_NUM_PAGES>;

/// A single-page log buffer.
///
/// Matches the size GSP-RM hardcodes for the root and RM state monitor tasks.
type SmallLogBuffer = LogBuffer<1>;

impl<const NUM_PAGES: usize> LogBuffer<NUM_PAGES> {
    /// Creates a new `LogBuffer` mapped on `dev`.
    fn new(dev: &device::Device<device::Bound>) -> Result<Self> {
        let obj = Self(Coherent::zeroed(dev, GFP_KERNEL)?);

        let start_addr = obj.0.dma_address();

        let pte_view = io_project!(
            obj.0,
            [build: 0][build: size_of::<u64>()..][build: ..NUM_PAGES * size_of::<u64>()]
        )
        .try_cast::<PteArray<NUM_PAGES>>()?;
        PteArray::init(pte_view, start_addr)?;

        Ok(obj)
    }
}

/// The log buffers GSP-RM writes its debug output to, one per LIBOS3 task.
///
/// The Open RM equivalents are `_kgspInitLibosLoggingStructures`, which allocates the buffers,
/// and `kgspSetupLibosInitArgs_IMPL`, which builds the `pLibosInitArgs[]` array.
struct LogBuffers {
    /// Init task.
    loginit: TaskLogBuffer,
    /// Interrupt task.
    logintr: TaskLogBuffer,
    /// RM task.
    logrm: TaskLogBuffer,
    /// MNOC task.
    logmnoc: TaskLogBuffer,
    /// Root task.
    logroot: SmallLogBuffer,
    /// RM state monitor task.
    logrmon: SmallLogBuffer,
}

/// GSP runtime data.
#[pin_data]
pub(crate) struct Gsp {
    /// Libos arguments.
    pub(crate) libos: Coherent<[LibosMemoryRegionInitArgument]>,
    /// Log buffers, optionally exposed via debugfs.
    #[pin]
    logs: debugfs::Scope<LogBuffers>,
    /// Command queue, borrowed by the GSP event interrupt handler.
    #[pin]
    pub(crate) cmdq: Cmdq,
    /// RM arguments.
    rmargs: Coherent<GspArgumentsPadded>,
    /// Buffer GSP-RM maps during init to report its own state.
    rm_state_monitor: Coherent<[u8; GSP_PAGE_SIZE]>,
}

impl Gsp {
    // Creates an in-place initializer for a `Gsp` manager for `pdev`.
    pub(crate) fn new(pdev: &pci::Device<device::Bound>) -> impl PinInit<Self, Error> + '_ {
        pin_init::pin_init_scope(move || {
            let dev = pdev.as_ref();

            let loginit = TaskLogBuffer::new(dev)?;
            let logintr = TaskLogBuffer::new(dev)?;
            let logrm = TaskLogBuffer::new(dev)?;
            let logmnoc = TaskLogBuffer::new(dev)?;
            let logroot = SmallLogBuffer::new(dev)?;
            let logrmon = SmallLogBuffer::new(dev)?;

            Ok(try_pin_init!(Self {
                cmdq <- Cmdq::new(dev),
                rmargs: Coherent::init(dev, GFP_KERNEL, GspArgumentsPadded::new(&cmdq))?,
                rm_state_monitor: Coherent::zeroed(dev, GFP_KERNEL)?,
                libos: {
                    let mut libos = CoherentBox::zeroed_slice(
                        dev,
                        GSP_PAGE_SIZE / size_of::<LibosMemoryRegionInitArgument>(),
                        GFP_KERNEL,
                    )?;

                    libos.init_at(0, LibosMemoryRegionInitArgument::new("LOGINIT", &loginit.0))?;
                    libos.init_at(1, LibosMemoryRegionInitArgument::new("LOGINTR", &logintr.0))?;
                    libos.init_at(2, LibosMemoryRegionInitArgument::new("LOGRM", &logrm.0))?;
                    libos.init_at(3, LibosMemoryRegionInitArgument::new("RMARGS", rmargs))?;

                    libos.into()
                },
                logs <- {
                    let log_buffers = LogBuffers {
                        loginit,
                        logintr,
                        logrm,
                        logmnoc,
                        logroot,
                        logrmon,
                    };

                    #[allow(static_mut_refs)]
                    // SAFETY: `DEBUGFS_ROOT` is created before driver registration and cleared
                    // after driver unregistration, so no probe() can race with its modification.
                    //
                    // PANIC: `DEBUGFS_ROOT` cannot be `None` here.  It is set before driver
                    // registration and cleared after driver unregistration, so it is always
                    // `Some` for the entire lifetime that probe() can be called.
                    let log_parent: &debugfs::Dir = unsafe { crate::DEBUGFS_ROOT.as_ref() }
                        .expect("DEBUGFS_ROOT not initialized");

                    log_parent.scope(log_buffers, dev.name(), |logs, dir| {
                        dir.read_binary_file(c"loginit", &logs.loginit.0);
                        dir.read_binary_file(c"logintr", &logs.logintr.0);
                        dir.read_binary_file(c"logrm", &logs.logrm.0);
                        dir.read_binary_file(c"logmnoc", &logs.logmnoc.0);
                        dir.read_binary_file(c"logroot", &logs.logroot.0);
                        dir.read_binary_file(c"logrmon", &logs.logrmon.0);
                    })
                },
            }))
        })
    }

    /// Query the GSP for the static GPU information.
    pub(crate) fn get_static_info(&self, bar: Bar0<'_>) -> Result<commands::GetGspStaticInfoReply> {
        self.cmdq.send_command(bar, commands::GetGspStaticInfo)
    }
}

/// Opaque bundle required to unload the GSP. Created by [`Gsp::boot`], consumed by [`Gsp::unload`].
pub(crate) struct UnloadBundle(KBox<dyn hal::UnloadBundle>);
