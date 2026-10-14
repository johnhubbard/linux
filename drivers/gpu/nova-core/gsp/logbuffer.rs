// SPDX-License-Identifier: GPL-2.0

//! GSP-RM log buffers, and the debugfs entries exposing them.

use core::convert::Infallible;

use kernel::{
    debugfs,
    device,
    dma::{
        Coherent,
        CoherentBox, //
    },
    fs::file,
    io::{
        io_project,
        Io, //
    },
    prelude::*,
    str::CString,
    sync::barrier::{
        dma_mb,
        Read, //
    },
    transmute::AsBytes,
    uaccess::UserSliceWriter, //
};

use crate::{
    firmware::gsp::{
        BuildId,
        BUILD_ID_MAX_LENGTH, //
    },
    gpu::Spec,
    gsp::{
        LibosMemoryRegionInitArgument,
        PteArray,
        GSP_PAGE_SIZE, //
    },
};

/// Number of GSP pages to use in a RM log buffer.
const RM_LOG_BUFFER_NUM_PAGES: usize = 0x10;

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

    /// Copies the contents of the debugfs file of this buffer, the dump header followed by the
    /// buffer, into memory that does not belong to the device.
    ///
    /// A buffer the GSP never wrote to yields an empty vector, as it holds nothing worth keeping.
    fn snapshot(&self) -> Result<VVec<u8>> {
        // Offset 0 holds the "put" pointer, which the GSP advances as it appends entries. It is
        // still zero if nothing was ever logged, which is all that is tested here: a buffer that
        // was written to is copied whole, and making sense of "put" is left to the decoder.
        let put = io_project!(self.buffer, [build: 0][build: ..size_of::<u64>()])
            .try_cast::<u64>()?
            .read_val();
        if put == 0 {
            return Ok(VVec::new());
        }

        // ORDERING: LOAD->LOAD ordering needed to order the "put" read before the data read. The
        // GSP has normally been stopped by the time this runs, but a boot that timed out can leave
        // it still appending.
        dma_mb(Read);

        let header = self
            .header
            .as_ref()
            .map_or(&[][..], |header| header.as_bytes());
        let mut snapshot = VVec::zeroed(header.len() + self.buffer.size(), GFP_KERNEL)?;
        let (snapshot_header, data) = snapshot.split_at_mut(header.len());
        snapshot_header.copy_from_slice(header);
        for (i, page) in data.chunks_exact_mut(GSP_PAGE_SIZE).enumerate() {
            io_project!(self.buffer, [try: i][build: ..]).copy_to_slice(page);
        }

        // The copy read "put" once more, and nothing orders that read against the rest of it, so
        // it may be newer than the data next to it. Keep the value read before the barrier, which
        // the data is at least as new as. Nothing done here can help a GSP that is still running
        // and wraps around over the oldest entries while they are copied; only stopping it could.
        data[..size_of::<u64>()].copy_from_slice(&put.to_ne_bytes());

        Ok(snapshot)
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
pub(super) struct LogBuffers<'a> {
    /// Device the buffers belong to. Also names their debugfs directory.
    dev: &'a device::Device<device::Bound>,
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
    pub(super) const COUNT: usize = 6;

    /// Allocates the six log buffers, mapped on `dev`, and gives each one the dump header for
    /// the GPU that `spec` describes when `build_id` is known.
    pub(super) fn new(
        dev: &'a device::Device<device::Bound>,
        spec: Spec,
        build_id: Option<&BuildId>,
    ) -> Result<Self> {
        let header = |task_prefix| build_id.map(|id| LogBufferHeader::new(spec, id, task_prefix));

        Ok(Self {
            dev,
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
    pub(super) fn init_arguments(
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

    /// Creates an initializer exposing these buffers under a directory named after their device.
    pub(super) fn scope(self) -> impl PinInit<debugfs::Scope<Self>, Infallible> + 'a {
        let dev = self.dev;

        let log_parent: &debugfs::Dir = crate::debugfs_data(dev).root();

        log_parent.scope(self, dev.name(), |logs, dir| logs.register_debugfs(dir))
    }

    /// Preserves whatever the GSP logged, so it can still be read once the GPU is gone.
    ///
    /// The buffers are DMA allocations of the device and cannot outlive it, so their contents are
    /// copied into memory owned by the module and exposed through fresh debugfs entries. Those
    /// live until the module is unloaded.
    ///
    /// Does nothing if `gsp_keep_logs` was not set when the module was loaded, as there is then
    /// no directory to put the copies in.
    fn retain(&self) -> Result {
        let data = crate::debugfs_data(self.dev);

        // The directory is taken here and the lock dropped again right away: what follows
        // allocates a copy of each of the six buffers, and no other device should have to wait
        // for that.
        let Some(dir) = data.retained_logs().lock().dir.clone() else {
            return Ok(());
        };

        let logs = RetainedLogBuffers {
            name: CString::try_from_fmt(fmt!("{}", self.dev.name()))?,
            loginit: self.loginit.snapshot()?,
            logintr: self.logintr.snapshot()?,
            logrm: self.logrm.snapshot()?,
            logmnoc: self.logmnoc.snapshot()?,
            logroot: self.logroot.snapshot()?,
            logrmon: self.logrmon.snapshot()?,
        };

        // Nothing was ever logged, so there is nothing to keep. A copy from an earlier run of
        // this device is deliberately left alone: logs from a run that failed are worth more
        // than the silence of one that did not. A run that did log something does replace it,
        // even where what it replaces came from a run that failed: nothing here can tell the two
        // apart, and keeping the older copy would mean never seeing anything newer.
        if logs.is_empty() {
            return Ok(());
        }

        // Make every allocation of our own that can fail before the previous copy of this device
        // is dropped, so that none of them failing can leave it with no logs at all.
        let scope = KBox::<debugfs::Scope<RetainedLogBuffers>>::new_uninit(GFP_KERNEL)?;

        let mut retained = data.retained_logs().lock();

        retained.gpus.reserve(1, GFP_KERNEL)?;

        // An earlier run of the same device may have left a copy behind, and its directory
        // carries the name about to be used again, so it has to go first. Nothing below can
        // fail, so the replacement is guaranteed to take its place.
        retained.gpus.retain(|gpu| *gpu.name != *logs.name);

        let scope = scope.write_pin_init(dir.scope(logs, self.dev.name(), |logs, dir| {
            logs.register_debugfs(dir)
        }))?;

        retained.gpus.push(scope, GFP_KERNEL)?;

        dev_dbg!(self.dev, "GSP-RM log buffers retained\n");

        Ok(())
    }
}

impl Drop for LogBuffers<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.retain() {
            dev_warn!(self.dev, "failed to retain GSP-RM log buffers: {:?}\n", e);
        }
    }
}

/// Copies of the log buffers of a GPU that is no longer around.
///
/// A copy holds the contents of the debugfs file of its buffer: the dump header, when the buffer
/// has one, followed by the buffer.
struct RetainedLogBuffers {
    /// Name of the device the buffers came from, which also names their directory.
    ///
    /// A copy rather than a reference to the device, so that a GPU that is gone does not stay
    /// allocated for as long as its logs are kept.
    name: CString,
    /// Copy of the init log buffer, empty if it was never written to.
    loginit: VVec<u8>,
    /// Copy of the interrupts log buffer, empty if it was never written to.
    logintr: VVec<u8>,
    /// Copy of the RM log buffer, empty if it was never written to.
    logrm: VVec<u8>,
    /// Copy of the MNOC log buffer, empty if it was never written to.
    logmnoc: VVec<u8>,
    /// Copy of the root log buffer, empty if it was never written to.
    logroot: VVec<u8>,
    /// Copy of the RM state monitor log buffer, empty if it was never written to.
    logrmon: VVec<u8>,
}

impl RetainedLogBuffers {
    /// Returns each copy, paired with the name of its debugfs file.
    fn files(&self) -> [(&'static CStr, &VVec<u8>); LogBuffers::COUNT] {
        [
            (c"loginit", &self.loginit),
            (c"logintr", &self.logintr),
            (c"logrm", &self.logrm),
            (c"logmnoc", &self.logmnoc),
            (c"logroot", &self.logroot),
            (c"logrmon", &self.logrmon),
        ]
    }

    /// Returns `true` if none of the buffers was ever written to.
    fn is_empty(&self) -> bool {
        self.files().iter().all(|(_, data)| data.is_empty())
    }

    /// Exposes each copy that is not empty as a binary file in `dir`.
    fn register_debugfs<'data>(&'data self, dir: &debugfs::ScopedDir<'data, '_>) {
        for (name, data) in self.files() {
            if !data.is_empty() {
                dir.read_binary_file(name, data);
            }
        }
    }
}

/// Log buffers of GPUs that are gone, and the debugfs entries exposing them.
///
/// The copies live under a `retained` directory of their own instead of next to the entries of
/// the GPUs that are actually bound, so that a device coming back does not find its name taken.
pub(crate) struct RetainedLogs {
    /// One entry per GPU.
    ///
    /// Declared before `dir`, as the copies live below it.
    gpus: KVec<Pin<KBox<debugfs::Scope<RetainedLogBuffers>>>>,
    /// Parent directory of all copies. `None` unless retaining was asked for.
    dir: Option<debugfs::Dir>,
}

impl RetainedLogs {
    /// Creates an empty set of retained log buffers, retaining disabled.
    pub(crate) const fn new() -> Self {
        Self {
            gpus: KVec::new(),
            dir: None,
        }
    }

    /// Creates the directory the copies will live in, enabling retaining.
    ///
    /// Does nothing without `CONFIG_DEBUG_FS`, where a [`debugfs::Dir`] is a zero-sized type and
    /// the copies could never be read back.
    pub(crate) fn enable(&mut self, parent: &debugfs::Dir) {
        if !cfg!(CONFIG_DEBUG_FS) {
            return;
        }

        self.dir = Some(parent.subdir(c"retained"));
    }
}
