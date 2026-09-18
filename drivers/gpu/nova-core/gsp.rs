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
    fs::file,
    io::{
        io_project,
        io_write,
        Io, //
    },
    pci,
    prelude::*,
    transmute::AsBytes,
    uaccess::UserSliceWriter, //
};

pub(crate) mod cmdq;
pub(crate) mod commands;
mod fw;
mod nvkv;
mod regs;

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
        gsp::{
            BuildId,
            BUILD_ID_MAX_LENGTH, //
        },
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

/// Longest task name in a log dump header, matching Open RM's `TASK_NAME_MAX_LENGTH`.
const TASK_NAME_MAX_LENGTH: usize = 8;

/// Header ahead of the log data in a debugfs dump, in the layout of Open RM's
/// `LIBOS_LOG_NVLOG_BUFFER_V2`.
///
/// The decoder uses the header only when `build_id` matches the build ID of its GSP firmware ELF.
#[repr(C)]
struct LogBufferHeader {
    /// Architecture code of the GPU, from the `NV_PMC_BOOT_42` architecture field.
    gpu_arch: u32,
    /// Implementation number of the GPU within its architecture.
    gpu_impl: u32,
    version: u32,
    /// Number of valid bytes in `build_id`.
    build_id_length: u32,
    /// Name of the LIBOS3 task, printed ahead of each decoded line.
    task_prefix: [u8; TASK_NAME_MAX_LENGTH],
    /// Value that the decoder adds to each timestamp, zero when unknown.
    local_to_global_timer_delta: u64,
    /// Build ID of the GSP firmware, zero-padded.
    build_id: [u8; BUILD_ID_MAX_LENGTH],
    /// `LIBOS_LOG_NVLOG_BUFFER_FLAG_*` bits.
    flags: u32,
    reserved: u32,
}

// SAFETY: `LogBufferHeader` is `repr(C)`, its integer and byte array fields leave no padding
// between or after them, and it has no interior mutability.
unsafe impl AsBytes for LogBufferHeader {}

impl LogBufferHeader {
    /// The `LIBOS_LOG_NVLOG_BUFFER_V2` layout.
    const VERSION: u32 = 2;
    /// `LIBOS_LOG_NVLOG_BUFFER_FLAG_PACKED_METADATA`: each log entry packs its argument count and
    /// task id into the word that holds its metadata address.
    const FLAG_PACKED_METADATA: u32 = 0x1;

    /// Builds the header for a dump of the `task_prefix` task's log, on the GPU that `spec`
    /// describes, whose GSP runs the firmware with `build_id`.
    fn new(spec: Spec, build_id: &BuildId, task_prefix: &str) -> Self {
        let mut header = Self {
            gpu_arch: spec.chipset.arch() as u32,
            gpu_impl: u32::from(spec.implementation),
            version: Self::VERSION,
            build_id_length: build_id.len(),
            task_prefix: [0; TASK_NAME_MAX_LENGTH],
            local_to_global_timer_delta: 0,
            build_id: *build_id.padded(),
            flags: Self::FLAG_PACKED_METADATA,
            reserved: 0,
        };

        // The last byte stays zero, so that the name is NUL-terminated for the decoder.
        let prefix = task_prefix.as_bytes();
        let len = prefix.len().min(TASK_NAME_MAX_LENGTH - 1);
        header.task_prefix[..len].copy_from_slice(&prefix[..len]);

        header
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
struct LogBuffer<'a, const NUM_PAGES: usize> {
    /// Header that a debugfs dump carries ahead of the data, present when the GSP firmware's
    /// build ID is known.
    header: Option<LogBufferHeader>,
    /// The buffer that GSP-RM logs into.
    buffer: Coherent<'a, [[u8; GSP_PAGE_SIZE]; NUM_PAGES]>,
}

/// A log buffer at the default size, [`RM_LOG_BUFFER_NUM_PAGES`] pages.
///
/// Matches the registry defaults for the init, interrupt, RM and MNOC tasks
/// (`NV_REG_STR_RM_GSP_LOG_BUFFER_SIZE_TASK_*_DEFAULT`).
type TaskLogBuffer<'a> = LogBuffer<'a, RM_LOG_BUFFER_NUM_PAGES>;

/// A single-page log buffer, the size of the logs of the root task and of the RM state monitor
/// task.
type SmallLogBuffer<'a> = LogBuffer<'a, 1>;

impl<'a, const NUM_PAGES: usize> LogBuffer<'a, NUM_PAGES> {
    /// Creates a new `LogBuffer` mapped on `dev`, whose debugfs dump opens with `header`.
    fn new(
        dev: &'a device::Device<device::Bound>,
        header: Option<LogBufferHeader>,
    ) -> Result<Self> {
        let buffer = Coherent::zeroed(dev, GFP_KERNEL)?;

        let start_addr = buffer.dma_address();

        let pte_view = io_project!(
            buffer,
            [build: 0][build: size_of::<u64>()..][build: ..NUM_PAGES * size_of::<u64>()]
        )
        .try_cast::<PteArray<NUM_PAGES>>()?;
        PteArray::init(pte_view, start_addr)?;

        Ok(Self { header, buffer })
    }
}

impl<const NUM_PAGES: usize> debugfs::BinaryWriter for LogBuffer<'_, NUM_PAGES> {
    fn write_to_slice(
        &self,
        writer: &mut UserSliceWriter,
        offset: &mut file::Offset,
    ) -> Result<usize> {
        if offset.is_negative() {
            return Err(EINVAL);
        }

        // An offset too large for a `usize` is past the end of the dump.
        let Ok(offset_val) = usize::try_from(*offset) else {
            return Ok(0);
        };

        let header = self
            .header
            .as_ref()
            .map_or(&[][..], |header| header.as_bytes());
        let total = header.len() + self.buffer.size();
        if offset_val >= total {
            return Ok(0);
        }

        let count = (total - offset_val).min(writer.len());
        let mut written = 0;

        // The header comes first, and a read that reaches past it continues in the buffer.
        if let Some(header_rest) = header.get(offset_val..) {
            written = header_rest.len().min(count);
            writer.write_slice(&header_rest[..written])?;
        }
        if written < count {
            let buffer_offset = offset_val + written - header.len();
            writer.write_dma(&self.buffer, buffer_offset, count - written)?;
            written = count;
        }

        *offset += i64::try_from(written)?;
        Ok(written)
    }
}

/// The log buffers to which GSP-RM writes its debug output, one per LIBOS3 task.
struct LogBuffers<'a> {
    /// Init task.
    loginit: TaskLogBuffer<'a>,
    /// Interrupt task.
    logintr: TaskLogBuffer<'a>,
    /// RM task.
    logrm: TaskLogBuffer<'a>,
    /// MNOC task.
    logmnoc: TaskLogBuffer<'a>,
    /// Root task.
    logroot: SmallLogBuffer<'a>,
    /// RM state monitor task.
    logrmon: SmallLogBuffer<'a>,
}

impl<'a> LogBuffers<'a> {
    /// Number of log buffers.
    const COUNT: usize = 6;

    /// Allocates the six log buffers, mapped on `dev`, and gives each one the dump header for
    /// the GPU that `spec` describes when `build_id` is known.
    fn new(
        dev: &'a device::Device<device::Bound>,
        spec: Spec,
        build_id: Option<&BuildId>,
    ) -> Result<Self> {
        let header = |task_prefix| build_id.map(|id| LogBufferHeader::new(spec, id, task_prefix));

        Ok(Self {
            loginit: TaskLogBuffer::new(dev, header("INIT"))?,
            logintr: TaskLogBuffer::new(dev, header("INTR"))?,
            logrm: TaskLogBuffer::new(dev, header("RM"))?,
            logmnoc: TaskLogBuffer::new(dev, header("MNOC"))?,
            logroot: SmallLogBuffer::new(dev, header("ROOT"))?,
            logrmon: SmallLogBuffer::new(dev, header("RMON"))?,
        })
    }

    /// Fills the first [`Self::COUNT`] entries of `libos` with the log buffers, under the names
    /// that GSP-RM looks them up by.
    fn init_arguments(
        &self,
        libos: &mut CoherentBox<'_, [LibosMemoryRegionInitArgument]>,
    ) -> Result {
        libos.init_at(
            0,
            LibosMemoryRegionInitArgument::new("LOGINIT", &self.loginit.buffer),
        )?;
        libos.init_at(
            1,
            LibosMemoryRegionInitArgument::new("LOGINTR", &self.logintr.buffer),
        )?;
        libos.init_at(
            2,
            LibosMemoryRegionInitArgument::new("LOGRM", &self.logrm.buffer),
        )?;
        libos.init_at(
            3,
            LibosMemoryRegionInitArgument::new("LOGMNOC", &self.logmnoc.buffer),
        )?;
        libos.init_at(
            4,
            LibosMemoryRegionInitArgument::new("LOGROOT", &self.logroot.buffer),
        )?;
        libos.init_at(
            5,
            LibosMemoryRegionInitArgument::new("LOGRMON", &self.logrmon.buffer),
        )?;

        Ok(())
    }

    /// Exposes each log buffer as a binary file in `dir`, under the lowercase form of its name.
    fn register_debugfs<'data>(&'data self, dir: &debugfs::ScopedDir<'data, '_>) {
        dir.read_binary_file(c"loginit", &self.loginit);
        dir.read_binary_file(c"logintr", &self.logintr);
        dir.read_binary_file(c"logrm", &self.logrm);
        dir.read_binary_file(c"logmnoc", &self.logmnoc);
        dir.read_binary_file(c"logroot", &self.logroot);
        dir.read_binary_file(c"logrmon", &self.logrmon);
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
                logs <- {
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
                        logs.register_debugfs(dir)
                    })
                },
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
