// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

pub(super) mod gmc;
// No caller sends an RPC command: GSP_INIT and GSP_SUSPEND replaced the RPC commands that the
// driver used during boot and shutdown. The r000 GSP-RM still accepts RPC commands once it is up,
// so the RPC send and reply paths stay, and are built as dead code.
#[expect(dead_code)]
pub(super) mod rpc;

use core::{
    cell::Cell,
    mem, //
};

use kernel::{
    device,
    dma::{
        Coherent,
        CoherentBox,
        DmaAddress, //
    },
    io::{
        io_project,
        poll::read_poll_timeout,
        register::Array,
        Io, //
    },
    new_mutex,
    num::casts::FromSafeCast,
    prelude::*,
    sync::{
        barrier::{
            dma_mb,
            Full, //
        },
        Mutex, //
    },
    time::{
        Delta,
        Instant,
        Monotonic, //
    },
    transmute::{
        AsBytes,
        FromBytes, //
    },
};

use pin_init::pin_init_scope;

use crate::{
    driver::Bar0,
    gsp::{
        fw::{
            MsgqTxHeader,
            QueueElementHeader,
            GSP_MSG_QUEUE_ELEMENT_SIZE_MAX, //
        },
        PteArray,
        GSP_PAGE_SHIFT,
        GSP_PAGE_SIZE, //
    },
    mctp::NvdmType,
    num, //
};

use super::regs;

/// Marker type representing the absence of a reply for a command. Commands using this as their
/// reply type are sent using [`Cmdq::send_rpc_command_no_wait`].
pub(crate) struct NoReply;

/// Trait implemented by types that can be sent as a single command queue element.
///
/// The command queue allocates `size()` bytes after the [`QueueElementHeader`] and calls `write()`
/// to fill them with the message header and the message.
trait CommandElement {
    /// NVDM type that the queue element header announces for this kind of message.
    const NVDM_TYPE: NvdmType;

    /// Size in bytes of the element, not including the [`QueueElementHeader`].
    fn size(&self) -> usize;

    /// Writes the message header and the contents of the command into `dst`. `dev` is the queue's
    /// device (to be used for logging), `seq` is the sequence number of the element.
    fn write(&self, dev: &device::Device, seq: u32, dst: &mut GspCommand<'_>) -> Result;
}

/// Number of GSP pages making the [`Msgq`].
pub(crate) const MSGQ_NUM_PAGES: u32 = 64;

// The queue pointers are wrapping `u32` counters, and the slot is
// `counter % MSGQ_NUM_PAGES`. For slots to remain contiguous when a counter
// wraps, `MSGQ_NUM_PAGES` must divide `1 << 32`.
static_assert!((1u64 << u32::BITS) % cv!(MSGQ_NUM_PAGES => u64) == 0);

/// Circular buffer of a [`Msgq`].
///
/// This area of memory is to be shared between the driver and the GSP to exchange commands or
/// messages.
#[repr(C, align(0x1000))]
#[derive(Debug)]
struct MsgqData {
    data: [[u8; GSP_PAGE_SIZE]; cv!(MSGQ_NUM_PAGES)],
}

// Annoyingly we are forced to use a literal to specify the alignment of
// `MsgqData`, so check that it corresponds to the actual GSP page size here.
static_assert!(align_of::<MsgqData>() == GSP_PAGE_SIZE);

/// Unidirectional message queue.
///
/// Contains the data for a message queue, that either the driver or GSP writes to.
///
/// The queue pointers are in BAR0 registers, so `tx` carries the queue's geometry alone. `msgq`
/// is aligned to [`GSP_PAGE_SIZE`], and the bytes between the two are zero padding.
#[repr(C)]
// There is no struct defined for this in the open-gpu-kernel-source headers.
// Instead it is defined by code in `GspMsgQueuesInit()`.
struct Msgq {
    /// The msgq TX header, which describes the queue to the GSP.
    tx: MsgqTxHeader,
    /// The message queue proper.
    msgq: MsgqData,
}

/// Structure shared between the driver and the GSP and containing the command and message queues.
#[repr(C)]
struct GspMem {
    /// Self-mapping page table entries.
    ptes: PteArray<{ Self::PTE_ARRAY_SIZE }>,
    /// CPU queue: the driver writes commands here, and the GSP reads them.
    ///
    /// This member is read-only for the GSP.
    cpuq: Msgq,
    /// GSP queue: the GSP writes messages here, and the driver reads them.
    ///
    /// This member is read-only for the driver.
    gspq: Msgq,
}

impl GspMem {
    const PTE_ARRAY_SIZE: usize = GSP_PAGE_SIZE / size_of::<u64>();
}

// SAFETY: These structs don't meet the no-padding requirements of AsBytes but
// that is not a problem because they are not used outside the kernel.
unsafe impl AsBytes for GspMem {}

// SAFETY: These structs don't meet the no-padding requirements of FromBytes but
// that is not a problem because they are not used outside the kernel.
unsafe impl FromBytes for GspMem {}

/// Wrapper around [`GspMem`] to share it with the GPU using a [`Coherent`].
///
/// This provides the low-level functionality to communicate with the GSP, including allocation of
/// queue space to write messages to and management of read/write pointers.
///
/// This is shared with the GSP, with clear ownership rules regarding the command queues:
///
/// * The driver owns (i.e. can write to) the part of the CPU message queue between the CPU write
///   pointer and the GSP read pointer. This region is returned by [`Self::driver_write_area`].
/// * The driver owns (i.e. can read from) the part of the GSP message queue between the CPU read
///   pointer and the GSP write pointer. This region is returned by [`Self::driver_read_area`].
///
/// Each pointer counts elements without wrapping at the ring size: a ring is empty when its two
/// pointers are equal and full when they differ by the ring size. A GSP reset zeroes all four
/// pointer registers, so the driver keeps its own two pointers here and writes them out. It also
/// keeps the last value that it read of each GSP pointer, and uses that value while the register
/// reads as zero.
struct DmaGspMem<'a> {
    /// The queues, mapped for the GSP.
    mem: Coherent<'a, GspMem>,
    /// MMIO mapping of PCI BAR0, for the queue pointer registers.
    bar: Bar0<'a>,
    /// Number of elements that the driver has written to the command queue, published to the GSP
    /// through `NV_PGSP_QUEUE_HEAD`.
    cpu_write_ptr: u32,
    /// Number of elements that the driver has consumed from the message queue, published to the
    /// GSP through `NV_PGSP_MSGQ_TAIL`.
    cpu_read_ptr: u32,
    /// The GSP's count of consumed commands, as last read from `NV_PGSP_QUEUE_TAIL`.
    last_gsp_read_ptr: Cell<u32>,
    /// The GSP's count of posted messages, as last read from `NV_PGSP_MSGQ_HEAD`.
    last_gsp_write_ptr: Cell<u32>,
}

impl<'a> DmaGspMem<'a> {
    /// Allocate a new instance and map it for `dev`.
    fn new(dev: &'a device::Device<device::Bound>, bar: Bar0<'a>) -> Result<Self> {
        const MSGQ_SIZE: u32 = cv!(size_of::<Msgq>());
        const MSG_SIZE: u32 = cv!(GSP_PAGE_SIZE);
        const ENTRY_OFF: u32 = cv!(mem::offset_of!(Msgq, msgq));

        let mut gsp_mem = CoherentBox::<'_, GspMem>::zeroed(dev, GFP_KERNEL)?;
        gsp_mem.cpuq.tx = MsgqTxHeader::new(MSGQ_SIZE, MSG_SIZE, MSGQ_NUM_PAGES, ENTRY_OFF);

        let gsp_mem: Coherent<'_, _> = gsp_mem.into();
        PteArray::init(io_project!(gsp_mem, .ptes), gsp_mem.dma_address())?;

        Ok(Self {
            mem: gsp_mem,
            bar,
            cpu_write_ptr: 0,
            cpu_read_ptr: 0,
            last_gsp_read_ptr: Cell::new(0),
            last_gsp_write_ptr: Cell::new(0),
        })
    }

    /// Returns the region of the CPU message queue that the driver is currently allowed to write
    /// to.
    ///
    /// As the message queue is a circular buffer, the region may be discontiguous in memory. In
    /// that case the second slice will have a non-zero length.
    fn driver_write_area(&mut self) -> (&mut [[u8; GSP_PAGE_SIZE]], &mut [[u8; GSP_PAGE_SIZE]]) {
        let avail = usize::from_safe_cast(self.cpuq_free_slots());
        let w_slot = usize::from_safe_cast(self.cpu_write_ptr % MSGQ_NUM_PAGES);

        // Number of slots from `w_slot` to the end of the ring, capped by `avail`.
        //
        // `w_slot` is below `MSGQ_NUM_PAGES` so the subtraction cannot underflow.
        let after_len = avail.min(cv!(MSGQ_NUM_PAGES => usize) - w_slot);
        // Remaining slots from the beginning of the ring (0 unless we wrap).
        //
        // Per the line above, `after_len` is less or equal to `avail` so the subtraction cannot
        // underflow.
        let before_len = avail - after_len;

        // PANIC: `w_slot + after_len` is at most `MSGQ_NUM_PAGES` by construction of
        // `after_len`.
        let after = io_project!(self.mem, .cpuq.msgq.data[panic: w_slot..w_slot + after_len]);
        // PANIC: `before_len` is at most `avail`, which is at most `MSGQ_NUM_PAGES`.
        let before = io_project!(self.mem, .cpuq.msgq.data[panic: ..before_len]);

        // SAFETY:
        // - Both regions are within the `avail` entries starting at `w_slot` that the driver owns,
        //   meaning the GSP won't touch them until the write pointer is advanced.
        // - There are no other references possible as `self` is mutably borrowed.
        // - The regions do not overlap: `avail` is at most `MSGQ_NUM_PAGES`, so `before_len` is
        //   at most `w_slot`.
        unsafe { (after.as_mut(), before.as_mut()) }
    }

    /// Returns the number of slots of the CPU message queue that the driver is currently allowed
    /// to write to: the ring size minus the elements that the GSP has not consumed yet.
    fn cpuq_free_slots(&self) -> u32 {
        let used = self.cpu_write_ptr.wrapping_sub(self.gsp_read_ptr());

        MSGQ_NUM_PAGES.saturating_sub(used)
    }

    /// Returns the size of the region of the CPU message queue that the driver is currently allowed
    /// to write to, in bytes.
    fn driver_write_area_size(&self) -> usize {
        num::u32_as_usize(self.cpuq_free_slots()) * GSP_PAGE_SIZE
    }

    /// Returns the number of slots of the GSP message queue that the driver is currently allowed
    /// to read from: the elements that the GSP has posted and the driver has not consumed yet.
    fn gspq_readable_slots(&self) -> u32 {
        self.gsp_write_ptr().wrapping_sub(self.cpu_read_ptr)
    }

    /// Returns the region of the GSP message queue that the driver is currently allowed to read
    /// from.
    ///
    /// As the message queue is a circular buffer, the region may be discontiguous in memory. In
    /// that case the second slice will have a non-zero length.
    fn driver_read_area(&self) -> (&[[u8; GSP_PAGE_SIZE]], &[[u8; GSP_PAGE_SIZE]]) {
        let avail = usize::from_safe_cast(self.gspq_readable_slots());
        let r_slot = usize::from_safe_cast(self.cpu_read_ptr % MSGQ_NUM_PAGES);

        // Number of slots from `r_slot` to the end of the ring, capped by `avail`.
        //
        // `r_slot` is below `MSGQ_NUM_PAGES` so the subtraction cannot underflow.
        let after_len = avail.min(cv!(MSGQ_NUM_PAGES => usize) - r_slot);
        // Remaining slots from the beginning of the ring (0 unless we wrap).
        //
        // Per the line above, `after_len` is less or equal to `avail` so the subtraction cannot
        // underflow.
        let before_len = avail - after_len;

        // PANIC: `r_slot + after_len` is at most `MSGQ_NUM_PAGES` by construction of
        // `after_len`.
        let after = io_project!(self.mem, .gspq.msgq.data[panic: r_slot..r_slot + after_len]);
        // PANIC: `before_len` is at most `avail`, which is at most `MSGQ_NUM_PAGES`.
        let before = io_project!(self.mem, .gspq.msgq.data[panic: ..before_len]);

        // SAFETY:
        // - Both regions are within the `avail` entries starting at `r_slot` that the driver owns,
        //   meaning the GSP won't touch them until the read pointer is advanced.
        // - There are no mutable references possible as `self` is borrowed.
        unsafe { (after.as_ref(), before.as_ref()) }
    }

    /// Allocates a region on the command queue that is large enough to send a command of `size`
    /// bytes, waiting for space to become available based on the provided timeout.
    ///
    /// This returns a [`GspCommand`] ready to be written to by the caller.
    ///
    /// # Errors
    ///
    /// - `EMSGSIZE` if the command is larger than [`GSP_MSG_QUEUE_ELEMENT_SIZE_MAX`].
    /// - `ETIMEDOUT` if space does not become available within the timeout.
    /// - `EIO` if the command header is not properly aligned.
    fn allocate_command(&mut self, size: usize, timeout: Delta) -> Result<GspCommand<'_>> {
        if size_of::<QueueElementHeader>() + size > GSP_MSG_QUEUE_ELEMENT_SIZE_MAX {
            return Err(EMSGSIZE);
        }
        read_poll_timeout(
            || Ok(self.driver_write_area_size()),
            |available_bytes| *available_bytes >= size_of::<QueueElementHeader>() + size,
            Delta::from_micros(1),
            timeout,
        )?;

        // Get the current writable area as an array of bytes.
        let (slice_1, slice_2) = {
            let (slice_1, slice_2) = self.driver_write_area();

            (slice_1.as_flattened_mut(), slice_2.as_flattened_mut())
        };

        // Extract area for the `QueueElementHeader`.
        let (header, slice_1) = QueueElementHeader::from_bytes_mut_prefix(slice_1).ok_or(EIO)?;

        // Create the contents area.
        let (slice_1, slice_2) = if slice_1.len() > size {
            // Contents fits entirely in `slice_1`.
            (&mut slice_1[..size], &mut slice_2[0..0])
        } else {
            // Need all of `slice_1` and some of `slice_2`.
            let slice_2_len = size - slice_1.len();
            (slice_1, &mut slice_2[..slice_2_len])
        };

        Ok(GspCommand {
            header,
            contents: (slice_1, slice_2),
        })
    }

    /// Returns the GSP's count of posted messages.
    fn gsp_write_ptr(&self) -> u32 {
        // ORDERING: `readl` ends in a read barrier, which orders this read before the data reads
        // that follow it.
        let ptr = u32::from(self.bar.read(regs::NV_PGSP_MSGQ_HEAD::at(0)).val());

        // A zeroed register is a GSP reset, not a count. Keep the last count read until GSP-RM
        // posts a message and writes the register back, unless the ring looked full at that
        // moment, in which case it is empty: the GSP has consumed every message that the driver
        // released to it.
        if ptr.wrapping_sub(self.cpu_read_ptr) > MSGQ_NUM_PAGES {
            let last = self.last_gsp_write_ptr.get();
            let ptr = if ptr == 0 && last.wrapping_sub(self.cpu_read_ptr) == MSGQ_NUM_PAGES {
                self.cpu_read_ptr
            } else {
                last
            };
            self.last_gsp_write_ptr.set(ptr);

            return ptr;
        }
        self.last_gsp_write_ptr.set(ptr);

        ptr
    }

    /// Returns the GSP's count of consumed commands.
    fn gsp_read_ptr(&self) -> u32 {
        let ptr = u32::from(self.bar.read(regs::NV_PGSP_QUEUE_TAIL::at(0)).address());

        // ORDERING: the data writes that follow must not pass this read. `readl` orders only the
        // reads that follow it, so the writes need a full barrier.
        dma_mb(Full);

        // A zeroed register is a GSP reset, not a count. Keep the last count read until GSP-RM
        // consumes a command and writes the register back, which undercounts the free space at
        // worst, unless the ring looked full at that moment, in which case it is empty: the GSP
        // has consumed every command.
        if self.cpu_write_ptr.wrapping_sub(ptr) > MSGQ_NUM_PAGES {
            let last = self.last_gsp_read_ptr.get();
            let ptr = if ptr == 0 && self.cpu_write_ptr.wrapping_sub(last) == MSGQ_NUM_PAGES {
                self.cpu_write_ptr
            } else {
                last
            };
            self.last_gsp_read_ptr.set(ptr);

            return ptr;
        }
        self.last_gsp_read_ptr.set(ptr);

        ptr
    }

    /// Releases `elem_count` more elements of the message queue to the GSP.
    fn advance_cpu_read_ptr(&mut self, elem_count: u32) {
        self.cpu_read_ptr = self.cpu_read_ptr.wrapping_add(elem_count);

        // ORDERING: the data reads must complete before this write releases the slots to the GSP.
        // `writel` orders only the writes before it, so the reads need a full barrier.
        dma_mb(Full);

        self.bar.write(
            Array::at(0),
            regs::NV_PGSP_MSGQ_TAIL::zeroed().with_val(self.cpu_read_ptr),
        );
    }

    /// Publishes `elem_count` more elements of the command queue to the GSP.
    ///
    /// The write to `NV_PGSP_QUEUE_HEAD` that publishes them also interrupts the GSP.
    fn advance_cpu_write_ptr(&mut self, elem_count: u32) {
        self.cpu_write_ptr = self.cpu_write_ptr.wrapping_add(elem_count);

        // ORDERING: `writel` begins with a write barrier, which orders the data writes before this
        // register write.
        self.bar.write(
            Array::at(0),
            regs::NV_PGSP_QUEUE_HEAD::zeroed().with_address(self.cpu_write_ptr),
        );
    }
}

/// A command ready to be sent on the command queue.
///
/// This is the type returned by [`DmaGspMem::allocate_command`].
struct GspCommand<'a> {
    // Writable reference to the queue element header of the command.
    header: &'a mut QueueElementHeader,
    // Writable slices to the contents of the command: the message header and the message. The
    // second slice is zero unless the command loops over the command queue.
    contents: (&'a mut [u8], &'a mut [u8]),
}

/// A message ready to be processed from the message queue.
///
/// This is the type returned by [`CmdqInner::wait_for_msg`].
struct GspMessage<'a> {
    // Reference to the queue element header of the message.
    header: &'a QueueElementHeader,
    // Slices to the contents of the message: the message header and the message, trimmed to the
    // length that the queue element header declares. The second slice is empty unless the message
    // loops over the message queue.
    contents: (&'a [u8], &'a [u8]),
}

/// A message whose protocol layer has been parsed and validated.
enum GspParsedMessage<'a> {
    /// A [RPC message](rpc::RpcMessage).
    Rpc(rpc::RpcMessage<'a>),
    /// A [GMC message](gmc::GmcMessage).
    Gmc(gmc::GmcMessage<'a>),
}

impl GspParsedMessage<'_> {
    /// Log the message on `dev`.
    fn log(&self, dev: &device::Device) {
        match self {
            GspParsedMessage::Rpc(rpc) => rpc.log(dev),
            GspParsedMessage::Gmc(gmc) => gmc.log(dev),
        }
    }
}

/// GSP command queue.
///
/// Provides the ability to send commands and receive messages from the GSP using a shared memory
/// area.
#[pin_data]
pub(crate) struct Cmdq<'cmdq>
where
    // Hide the mutex's invariant resource lifetime so `Cmdq` remains covariant.
    exists<'gpu>: 'cmdq,
{
    /// Inner mutex-protected state.
    #[pin]
    #[uses('gpu: invariant)]
    inner: Mutex<CmdqInner<'gpu>>,
    /// DMA address of the command queue's shared memory region.
    pub(super) dma_addr: DmaAddress,
}

impl<'cmdq> Cmdq<'cmdq> {
    /// Offset of the data after the PTEs.
    const POST_PTE_OFFSET: usize = core::mem::offset_of!(GspMem, cpuq);

    /// Offset of command queue ring buffer.
    pub(crate) const CMDQ_OFFSET: usize = core::mem::offset_of!(GspMem, cpuq)
        + core::mem::offset_of!(Msgq, msgq)
        - Self::POST_PTE_OFFSET;

    /// Offset of message queue ring buffer.
    pub(crate) const STATQ_OFFSET: usize = core::mem::offset_of!(GspMem, gspq)
        + core::mem::offset_of!(Msgq, msgq)
        - Self::POST_PTE_OFFSET;

    /// Number of page table entries for the GSP shared region.
    pub(crate) const NUM_PTES: usize = size_of::<GspMem>() >> GSP_PAGE_SHIFT;

    /// Default timeout for receiving a message from the GSP.
    pub(super) const RECEIVE_TIMEOUT: Delta = Delta::from_secs(5);

    /// Creates a new command queue for `dev`.
    pub(crate) fn new(
        dev: &'cmdq device::Device<device::Bound>,
        bar: Bar0<'cmdq>,
    ) -> impl PinInit<Self, Error> + 'cmdq {
        pin_init_scope(move || {
            let gsp_mem = DmaGspMem::new(dev, bar)?;

            Ok(try_pin_init!(Self {
                dma_addr: gsp_mem.mem.dma_address(),
                inner <- new_mutex!(CmdqInner {
                    dev,
                    gsp_mem,
                    seq: 0,
                }),
            }))
        })
    }

    /// Runs `f` with the command queue locked.
    fn with_inner_locked<R>(&self, f: impl FnOnce(&mut CmdqInner<'_>) -> R) -> R {
        self.with_inner(|inner| f(&mut inner.lock()))
    }

    /// Logs and consumes every message the GSP has already posted, and returns without waiting for
    /// more.
    ///
    /// No caller is waiting for a reply while this holds the queue mutex, so every message is
    /// logged as an event. See "Draining the GSP-to-CPU queue" in
    /// `Documentation/gpu/nova/core/interrupts.rst`.
    ///
    /// # Errors
    ///
    /// `EIO` if a message fails framing validation.
    pub(crate) fn drain(&self) -> Result {
        self.with_inner_locked(|inner| inner.drain())
    }
}

/// Inner mutex protected state of [`Cmdq`].
struct CmdqInner<'a> {
    /// Device this command queue belongs to.
    dev: &'a device::Device,
    /// Current command sequence number.
    seq: u32,
    /// Memory area shared with the GSP for communicating commands and messages.
    gsp_mem: DmaGspMem<'a>,
}

impl CmdqInner<'_> {
    /// Timeout for waiting for space on the command queue.
    const ALLOCATE_TIMEOUT: Delta = Delta::from_secs(1);

    /// Allocate enough space on the command queue to store `command`, initialize it with
    /// [`CommandElement::write`], and send the command to the GSP.
    ///
    /// Returns the sequence number of the sent element.
    ///
    /// # Errors
    ///
    /// - `EMSGSIZE` if the command exceeds the maximum queue element size.
    /// - `ETIMEDOUT` if space does not become available within the timeout.
    ///
    /// Error codes returned by [`CommandElement::write`] are returned as-is.
    fn send_command_element<E: CommandElement>(&mut self, command: E) -> Result<u32> {
        let size_in_bytes = command.size();
        let seq = self.seq;
        let mut dst = self
            .gsp_mem
            .allocate_command(size_in_bytes, Self::ALLOCATE_TIMEOUT)?;

        // Fill the queue element header.
        let header_init = QueueElementHeader::init(E::NVDM_TYPE, size_in_bytes);
        // SAFETY: `dst.header` is a valid reference, and is not touched if the initializer fails.
        unsafe {
            pin_init::raw_try_init(core::ptr::from_mut(dst.header), header_init)?;
        }

        // Write the message header and payload.
        command.write(self.dev, seq, &mut dst)?;

        // All set - update the write pointer and inform the GSP of the new command.
        let elem_count = dst.header.element_count();
        self.seq = self.seq.wrapping_add(1);
        self.gsp_mem.advance_cpu_write_ptr(elem_count);

        Ok(seq)
    }

    /// Wait for a message to become available on the message queue.
    ///
    /// This works purely at the transport layer: it validates the [`QueueElementHeader`] of the
    /// message and trims the contents to the length that it declares, and does not interpret the
    /// message header that follows.
    ///
    /// This method returns:
    ///
    /// - A reference to the [`QueueElementHeader`] of the message,
    /// - Two byte slices with the contents of the message. The second slice is empty unless the
    ///   message loops across the message queue.
    ///
    /// # Errors
    ///
    /// - `ETIMEDOUT` if `timeout` has elapsed before any message becomes available.
    /// - `EIO` if a message fails framing validation.
    fn wait_for_msg(&self, timeout: Delta) -> Result<GspMessage<'_>> {
        // Wait for a message to arrive from the GSP.
        let (slice_1, slice_2) = if timeout == Delta::ZERO {
            // If `timeout == Delta::ZERO`, check the message queue once as `read_poll_timeout` can
            // potentially sleep if the clock hasn't moved forward while the condition was being
            // checked.
            let slices = self.gsp_mem.driver_read_area();
            if !slices.0.is_empty() {
                Ok(slices)
            } else {
                Err(ETIMEDOUT)
            }
        } else {
            read_poll_timeout(
                || Ok(self.gsp_mem.driver_read_area()),
                |driver_area| !driver_area.0.is_empty(),
                Delta::from_micros(1),
                timeout,
            )
        }
        .map(|(slice_1, slice_2)| (slice_1.as_flattened(), slice_2.as_flattened()))?;

        // Extract the `QueueElementHeader`.
        let (header, slice_1) = QueueElementHeader::from_bytes_prefix(slice_1).ok_or(EIO)?;

        header.validate().inspect_err(|_| {
            dev_err!(&self.dev, "GSP receive: bad queue element header\n");
        })?;

        // Cut the message slices down to the length that the queue element header declares.
        let contents =
            trim_message_slice((slice_1, slice_2), header.message_len()).inspect_err(|_| {
                dev_err!(
                    &self.dev,
                    "GSP receive: read area shorter than the message\n"
                );
            })?;

        Ok(GspMessage { header, contents })
    }

    /// Wait for the next element on the message queue, pass it to `f`, advance the read pointer
    /// past the message, and return the result of `f`.
    ///
    /// The read pointer advances regardless of whether `f` succeeds or not.
    ///
    /// # Errors
    ///
    /// - `ETIMEDOUT` if `timeout` has elapsed before any message becomes available.
    /// - `EIO` if a message fails framing validation, or carries an NVDM type that the queue does
    ///   not use. In this case, the read pointer does not advance and the queue is stalled. This
    ///   is a fatal error.
    ///
    /// If an error is returned, then `f` has not been called.
    ///
    /// Note that `R` may itself be a [`Result`]. In this case, the above only applies to the outer
    /// [`Result`] returned by this method.
    fn consume_element<R>(
        &mut self,
        timeout: Delta,
        f: impl FnOnce(GspParsedMessage<'_>) -> R,
    ) -> Result<R> {
        let message = self.wait_for_msg(timeout)?;
        let elem_count = message.header.element_count();

        // The NVDM type of the queue element header selects the message header that follows.
        let message = match message.header.nvdm_type() {
            Ok(NvdmType::RmRpc) => {
                GspParsedMessage::Rpc(rpc::RpcMessage::parse(self.dev, message)?)
            }
            Ok(NvdmType::GmcApi) => {
                GspParsedMessage::Gmc(gmc::GmcMessage::parse(self.dev, message)?)
            }
            nvdm_type => {
                dev_err!(
                    &self.dev,
                    "GSP receive: unexpected NVDM type {:?}\n",
                    nvdm_type
                );
                return Err(EIO);
            }
        };

        let res = f(message);

        // Advance the read pointer past this message.
        self.gsp_mem.advance_cpu_read_ptr(elem_count);

        Ok(res)
    }

    /// Consumes messages from the queue until a message that satisfies `f` is encountered, and
    /// returns the result of `f`.
    ///
    /// The message is waited for up to `timeout` from the call.
    ///
    /// `f` is expected to return `Ok(Some(_))` with the result to return to the caller, and
    /// `Ok(None)` to signal that the received message was not the one expected (such messages are
    /// dropped). Errors returned by `f` are propagated to the caller.
    ///
    /// # Errors
    ///
    /// - `ETIMEDOUT` if no message satisfying `f` arrives before the deadline, however many other
    ///   messages arrive while waiting.
    /// - `EIO` if a message fails framing validation.
    /// - Any error returned by `f`.
    fn await_msg_with<R>(
        &mut self,
        timeout: Delta,
        f: impl Fn(GspParsedMessage<'_>) -> Result<Option<R>>,
    ) -> Result<R> {
        let deadline = Instant::<Monotonic>::now() + timeout;
        loop {
            let remaining = deadline - Instant::<Monotonic>::now();
            if remaining.is_negative() {
                break Err(ETIMEDOUT);
            }
            if let Some(m) = self.consume_element(remaining, &f)?? {
                break Ok(m);
            }
        }
    }

    /// Logs and consumes every message the queue holds.
    ///
    /// # Errors
    ///
    /// `EIO` if a message fails framing validation.
    fn drain(&mut self) -> Result {
        loop {
            match self.consume_element(Delta::ZERO, |message| message.log(self.dev)) {
                Ok(()) => (),
                // No more message pending on the queue, drain is complete.
                Err(ETIMEDOUT) => return Ok(()),
                // Other errors are fatal, and stall the queue.
                Err(e) => return Err(e),
            }
        }
    }
}

/// Helper function that trims the concatenation of `slice_1` and `slice_2` to `length`.
///
/// Returns the truncated `(slice_1, slice_2)` pair, or `EIO` if `length > slice_1.len() +
/// slice_2.len()`.
fn trim_message_slice<'a>(
    (slice_1, slice_2): (&'a [u8], &'a [u8]),
    length: usize,
) -> Result<(&'a [u8], &'a [u8])> {
    // See if `slice_1` covers the whole message, take it entirely if it doesn't.
    let slice_1 = slice_1.get(..length).unwrap_or(slice_1);
    // `slice_2` is the remainder of the message not covered by `slice_1`.
    // PANIC: The line above guarantees that `slice_1.len() <= length`, so the subtraction cannot
    // underflow.
    let slice_2 = slice_2.get(..length - slice_1.len()).ok_or(EIO)?;

    Ok((slice_1, slice_2))
}
