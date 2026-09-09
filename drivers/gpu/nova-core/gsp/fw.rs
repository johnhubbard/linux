// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

pub(crate) mod commands;
mod r000_00;

// Alias to avoid repeating the version number with every use.
use r000_00 as bindings;

use core::ops::Range;

use kernel::{
    bitfield,
    dma::Coherent,
    io::io_write,
    prelude::*,
    ptr::{
        Alignable,
        Alignment,
        KnownSize, //
    },
    sizes::{
        SizeConstants,
        SZ_128K, //
    },
    transmute::{
        AsBytes,
        FromBytes, //
    },
};

use crate::{
    fb::{
        FbRanges,
        FbSizes, //
    },
    firmware::{
        bindata::UcodesImage,
        gsp::GspFirmware, //
    },
    gpu::{
        Architecture,
        Chipset, //
    },
    gsp::{cmdq::Cmdq, GSP_PAGE_SHIFT, GSP_PAGE_SIZE},
    mctp::{
        MctpHeader,
        NvdmHeader,
        NvdmType, //
    },
    num::{
        self,
        FromSafeCast, //
    },
};

/// Maximum size of a single GSP message queue element in bytes.
///
/// GSP-RM reads this value at run time from the message queue init arguments rather than from a
/// build-time constant, so the driver chooses it, and this constant is the one copy.
pub(crate) const GSP_MSG_QUEUE_ELEMENT_SIZE_MAX: usize = GSP_PAGE_SIZE * 16;

/// Empty type to group methods related to heap parameters for running the GSP firmware.
enum GspFwHeapParams {}

/// Minimum required alignment for the GSP heap.
const GSP_HEAP_ALIGNMENT: Alignment = Alignment::new::<{ 1 << 20 }>();

impl GspFwHeapParams {
    /// Returns the amount of GSP-RM heap memory used during GSP-RM boot and initialization (up to
    /// and including the first client subdevice allocation).
    fn base_rm_size(chipset: Chipset) -> u64 {
        match chipset.arch() {
            Architecture::Turing | Architecture::Ampere | Architecture::Ada => {
                u64::from(bindings::GSP_FW_HEAP_PARAM_BASE_RM_SIZE_TU10X)
            }
            Architecture::Hopper | Architecture::BlackwellGB10x | Architecture::BlackwellGB20x => {
                u64::from(bindings::GSP_FW_HEAP_PARAM_BASE_RM_SIZE_GH100)
            }
        }
    }

    /// Returns the amount of heap memory required to support a single channel allocation.
    fn client_alloc_size() -> u64 {
        u64::from(bindings::GSP_FW_HEAP_PARAM_CLIENT_ALLOC_SIZE)
            .align_up(GSP_HEAP_ALIGNMENT)
            .unwrap_or(u64::MAX)
    }

    /// Returns the amount of memory to reserve for management purposes for a framebuffer of size
    /// `fb_size`.
    fn management_overhead(fb_size: u64) -> Result<u64> {
        let fb_size_gb = fb_size.div_ceil(u64::SZ_1G);

        u64::from(bindings::GSP_FW_HEAP_PARAM_SIZE_PER_GB)
            .checked_mul(fb_size_gb)
            .ok_or(EINVAL)?
            .align_up(GSP_HEAP_ALIGNMENT)
            .ok_or(EINVAL)
    }
}

/// Heap memory requirements and constraints for a given version of the GSP LIBOS.
pub(crate) struct LibosParams {
    /// The base amount of heap required by the GSP operating system, in bytes.
    carveout_size: u64,
    /// The minimum and maximum sizes allowed for the GSP FW heap, in bytes.
    allowed_heap_size: Range<u64>,
}

impl LibosParams {
    /// Version 2 of the GSP LIBOS (Turing and GA100)
    const LIBOS2: LibosParams = LibosParams {
        carveout_size: num::u32_as_u64(bindings::GSP_FW_HEAP_PARAM_OS_SIZE_LIBOS2),
        allowed_heap_size: num::u32_as_u64(bindings::GSP_FW_HEAP_SIZE_OVERRIDE_LIBOS2_MIN_MB)
            * u64::SZ_1M
            ..num::u32_as_u64(bindings::GSP_FW_HEAP_SIZE_OVERRIDE_LIBOS2_MAX_MB) * u64::SZ_1M,
    };

    /// Version 3 of the GSP LIBOS (GA102+)
    const LIBOS3: LibosParams = LibosParams {
        carveout_size: num::u32_as_u64(bindings::GSP_FW_HEAP_PARAM_OS_SIZE_LIBOS3_BAREMETAL),
        allowed_heap_size: num::u32_as_u64(
            bindings::GSP_FW_HEAP_SIZE_OVERRIDE_LIBOS3_BAREMETAL_MIN_MB,
        ) * u64::SZ_1M
            ..num::u32_as_u64(bindings::GSP_FW_HEAP_SIZE_OVERRIDE_LIBOS3_BAREMETAL_MAX_MB)
                * u64::SZ_1M,
    };

    /// Returns the libos parameters corresponding to `chipset`.
    pub(crate) fn from_chipset(chipset: Chipset) -> &'static LibosParams {
        if chipset < Chipset::GA102 {
            &Self::LIBOS2
        } else {
            &Self::LIBOS3
        }
    }

    /// Returns the WPR heap size to reserve when vGPU is enabled.
    pub(crate) fn vgpu_wpr_heap_size() -> u64 {
        u64::from(bindings::GSP_FW_HEAP_SIZE_VGPU_DEFAULT)
    }

    /// Returns the amount of memory (in bytes) to allocate for the WPR heap for a framebuffer size
    /// of `fb_size` (in bytes) for `chipset`.
    pub(crate) fn wpr_heap_size(&self, chipset: Chipset, fb_size: u64) -> Result<u64> {
        // The WPR heap will contain the following:
        // LIBOS carveout,
        Ok(self
            .carveout_size
            // RM boot working memory,
            .saturating_add(GspFwHeapParams::base_rm_size(chipset))
            // One RM client,
            .saturating_add(GspFwHeapParams::client_alloc_size())
            // Overhead for memory management.
            .saturating_add(GspFwHeapParams::management_overhead(fb_size)?)
            // Clamp to the supported heap sizes.
            .clamp(self.allowed_heap_size.start, self.allowed_heap_size.end - 1))
    }
}

/// Structure passed to the GSP bootloader, containing the framebuffer layout as well as the DMA
/// addresses of the GSP bootloader and firmware.
#[repr(transparent)]
pub(crate) struct GspFwWprMeta {
    inner: bindings::GspFwWprMeta,
}

// SAFETY: Padding is explicit and does not contain uninitialized data.
unsafe impl AsBytes for GspFwWprMeta {}

// SAFETY: This struct only contains integer types for which all bit patterns
// are valid.
unsafe impl FromBytes for GspFwWprMeta {}

type GspFwWprMetaBootResumeInfo = bindings::GspFwWprMeta__bindgen_ty_1;
type GspFwWprMetaBootInfo = bindings::GspFwWprMeta__bindgen_ty_1__bindgen_ty_1;

impl GspFwWprMeta {
    /// Returns an initializer for a `GspFwWprMeta` suitable for booting `gsp_firmware` using the
    /// framebuffer ranges `ranges`.
    pub(crate) fn from_ranges<'a>(
        gsp_firmware: &'a GspFirmware<'_>,
        ranges: &'a FbRanges,
    ) -> impl Init<Self> + 'a {
        let init_inner = init!(bindings::GspFwWprMeta {
            // CAST: we want to store the bits of `GSP_FW_WPR_META_MAGIC` unmodified.
            magic: bindings::GSP_FW_WPR_META_MAGIC as u64,
            revision: u64::from(bindings::GSP_FW_WPR_META_REVISION),
            sysmemAddrOfRadix3Elf: gsp_firmware.radix3_dma_address(),
            sizeOfRadix3Elf: u64::from_safe_cast(gsp_firmware.size()),
            sysmemAddrOfBootloader: gsp_firmware.bootloader.ucode.dma_address(),
            sizeOfBootloader: u64::from_safe_cast(gsp_firmware.bootloader.ucode.size()),
            bootloaderCodeOffset: u64::from(gsp_firmware.bootloader.code_offset),
            bootloaderDataOffset: u64::from(gsp_firmware.bootloader.data_offset),
            bootloaderManifestOffset: u64::from(gsp_firmware.bootloader.manifest_offset),
            __bindgen_anon_1: GspFwWprMetaBootResumeInfo {
                __bindgen_anon_1: GspFwWprMetaBootInfo {
                    sysmemAddrOfSignature: gsp_firmware.signatures.dma_address(),
                    sizeOfSignature: u64::from_safe_cast(gsp_firmware.signatures.size()),
                },
            },
            gspFwRsvdStart: ranges.non_wpr_heap.start,
            nonWprHeapOffset: ranges.non_wpr_heap.start,
            nonWprHeapSize: ranges.non_wpr_heap.len(),
            gspFwWprStart: ranges.wpr2.start,
            gspFwHeapOffset: ranges.wpr2_heap.start,
            gspFwHeapSize: ranges.wpr2_heap.len(),
            gspFwOffset: ranges.fw_image.start,
            bootBinOffset: ranges.boot.start,
            frtsOffset: ranges.frts.start,
            frtsSize: ranges.frts.len(),
            gspFwWprEnd: ranges
                .vga_workspace
                .start
                .align_down(Alignment::new::<SZ_128K>()),
            gspFwHeapVfPartitionCount: ranges.vf_partition_count,
            fbSize: ranges.fb.len(),
            vgaWorkspaceOffset: ranges.vga_workspace.start,
            vgaWorkspaceSize: ranges.vga_workspace.len(),
            pmuReservedSize: ranges.pmu_reserved_size,
            ..Zeroable::init_zeroed()
        });

        init!(GspFwWprMeta {
            inner <- init_inner,
        })
    }

    /// Returns an initializer for a `GspFwWprMeta` suitable for booting `gsp_firmware` using the
    /// framebuffer region sizes `sizes`.
    ///
    /// The region offsets are left at zero: the ACR ucode computes them when it sets up WPR2.
    pub(crate) fn from_sizes<'a>(
        gsp_firmware: &'a GspFirmware<'_>,
        sizes: &'a FbSizes,
    ) -> impl Init<Self> + 'a {
        /// VGA workspace size to reserve at the end of the framebuffer, in bytes.
        const VGA_WORKSPACE_SIZE: u64 = u64::SZ_128K;

        let init_inner = init!(bindings::GspFwWprMeta {
            // CAST: we want to store the bits of `GSP_FW_WPR_META_MAGIC` unmodified.
            magic: bindings::GSP_FW_WPR_META_MAGIC as u64,
            revision: u64::from(bindings::GSP_FW_WPR_META_REVISION),
            sysmemAddrOfRadix3Elf: gsp_firmware.radix3_dma_address(),
            sizeOfRadix3Elf: u64::from_safe_cast(gsp_firmware.size()),
            sysmemAddrOfBootloader: gsp_firmware.bootloader.ucode.dma_address(),
            sizeOfBootloader: u64::from_safe_cast(gsp_firmware.bootloader.ucode.size()),
            bootloaderCodeOffset: u64::from(gsp_firmware.bootloader.code_offset),
            bootloaderDataOffset: u64::from(gsp_firmware.bootloader.data_offset),
            bootloaderManifestOffset: u64::from(gsp_firmware.bootloader.manifest_offset),
            __bindgen_anon_1: GspFwWprMetaBootResumeInfo {
                __bindgen_anon_1: GspFwWprMetaBootInfo {
                    sysmemAddrOfSignature: gsp_firmware.signatures.dma_address(),
                    sizeOfSignature: u64::from_safe_cast(gsp_firmware.signatures.size()),
                },
            },
            nonWprHeapSize: sizes.non_wpr_heap_size,
            gspFwHeapSize: sizes.wpr2_heap_size,
            frtsSize: sizes.frts_size,
            gspFwHeapVfPartitionCount: sizes.vf_partition_count,
            vgaWorkspaceSize: VGA_WORKSPACE_SIZE,
            pmuReservedSize: sizes.pmu_reserved_size,
            ..Zeroable::init_zeroed()
        });

        init!(GspFwWprMeta {
            inner <- init_inner,
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u32)]
pub(crate) enum MsgFunction {
    // Common function codes
    AllocChannelDma = bindings::NV_VGPU_MSG_FUNCTION_ALLOC_CHANNEL_DMA,
    AllocCtxDma = bindings::NV_VGPU_MSG_FUNCTION_ALLOC_CTX_DMA,
    AllocDevice = bindings::NV_VGPU_MSG_FUNCTION_ALLOC_DEVICE,
    AllocMemory = bindings::NV_VGPU_MSG_FUNCTION_ALLOC_MEMORY,
    AllocObject = bindings::NV_VGPU_MSG_FUNCTION_ALLOC_OBJECT,
    AllocRoot = bindings::NV_VGPU_MSG_FUNCTION_ALLOC_ROOT,
    BindCtxDma = bindings::NV_VGPU_MSG_FUNCTION_BIND_CTX_DMA,
    ContinuationRecord = bindings::NV_VGPU_MSG_FUNCTION_CONTINUATION_RECORD,
    Free = bindings::NV_VGPU_MSG_FUNCTION_FREE,
    GetGspStaticInfo = bindings::NV_VGPU_MSG_FUNCTION_GET_GSP_STATIC_INFO,
    GetStaticInfo = bindings::NV_VGPU_MSG_FUNCTION_GET_STATIC_INFO,
    GspInitPostObjGpu = bindings::NV_VGPU_MSG_FUNCTION_GSP_INIT_POST_OBJGPU,
    GspRmControl = bindings::NV_VGPU_MSG_FUNCTION_GSP_RM_CONTROL,
    GspSetSystemInfo = bindings::NV_VGPU_MSG_FUNCTION_GSP_SET_SYSTEM_INFO,
    Log = bindings::NV_VGPU_MSG_FUNCTION_LOG,
    MapMemory = bindings::NV_VGPU_MSG_FUNCTION_MAP_MEMORY,
    Nop = bindings::NV_VGPU_MSG_FUNCTION_NOP,
    SetGuestSystemInfo = bindings::NV_VGPU_MSG_FUNCTION_SET_GUEST_SYSTEM_INFO,
    SetRegistry = bindings::NV_VGPU_MSG_FUNCTION_SET_REGISTRY,
    UnloadingGuestDriver = bindings::NV_VGPU_MSG_FUNCTION_UNLOADING_GUEST_DRIVER,

    // Event codes
    GspInitDone = bindings::NV_VGPU_MSG_EVENT_GSP_INIT_DONE,
    GspLockdownNotice = bindings::NV_VGPU_MSG_EVENT_GSP_LOCKDOWN_NOTICE,
    GspPostNoCat = bindings::NV_VGPU_MSG_EVENT_GSP_POST_NOCAT_RECORD,
    MmuFaultQueued = bindings::NV_VGPU_MSG_EVENT_MMU_FAULT_QUEUED,
    OsErrorLog = bindings::NV_VGPU_MSG_EVENT_OS_ERROR_LOG,
    PostEvent = bindings::NV_VGPU_MSG_EVENT_POST_EVENT,
    RcTriggered = bindings::NV_VGPU_MSG_EVENT_RC_TRIGGERED,
    UcodeLibOsPrint = bindings::NV_VGPU_MSG_EVENT_UCODE_LIBOS_PRINT,
}

impl TryFrom<u32> for MsgFunction {
    type Error = kernel::error::Error;

    fn try_from(value: u32) -> Result<MsgFunction> {
        match value {
            // Common function codes
            bindings::NV_VGPU_MSG_FUNCTION_ALLOC_CHANNEL_DMA => Ok(MsgFunction::AllocChannelDma),
            bindings::NV_VGPU_MSG_FUNCTION_ALLOC_CTX_DMA => Ok(MsgFunction::AllocCtxDma),
            bindings::NV_VGPU_MSG_FUNCTION_ALLOC_DEVICE => Ok(MsgFunction::AllocDevice),
            bindings::NV_VGPU_MSG_FUNCTION_ALLOC_MEMORY => Ok(MsgFunction::AllocMemory),
            bindings::NV_VGPU_MSG_FUNCTION_ALLOC_OBJECT => Ok(MsgFunction::AllocObject),
            bindings::NV_VGPU_MSG_FUNCTION_ALLOC_ROOT => Ok(MsgFunction::AllocRoot),
            bindings::NV_VGPU_MSG_FUNCTION_BIND_CTX_DMA => Ok(MsgFunction::BindCtxDma),
            bindings::NV_VGPU_MSG_FUNCTION_CONTINUATION_RECORD => {
                Ok(MsgFunction::ContinuationRecord)
            }
            bindings::NV_VGPU_MSG_FUNCTION_FREE => Ok(MsgFunction::Free),
            bindings::NV_VGPU_MSG_FUNCTION_GET_GSP_STATIC_INFO => Ok(MsgFunction::GetGspStaticInfo),
            bindings::NV_VGPU_MSG_FUNCTION_GET_STATIC_INFO => Ok(MsgFunction::GetStaticInfo),
            bindings::NV_VGPU_MSG_FUNCTION_GSP_INIT_POST_OBJGPU => {
                Ok(MsgFunction::GspInitPostObjGpu)
            }
            bindings::NV_VGPU_MSG_FUNCTION_GSP_RM_CONTROL => Ok(MsgFunction::GspRmControl),
            bindings::NV_VGPU_MSG_FUNCTION_GSP_SET_SYSTEM_INFO => Ok(MsgFunction::GspSetSystemInfo),
            bindings::NV_VGPU_MSG_FUNCTION_LOG => Ok(MsgFunction::Log),
            bindings::NV_VGPU_MSG_FUNCTION_MAP_MEMORY => Ok(MsgFunction::MapMemory),
            bindings::NV_VGPU_MSG_FUNCTION_NOP => Ok(MsgFunction::Nop),
            bindings::NV_VGPU_MSG_FUNCTION_SET_GUEST_SYSTEM_INFO => {
                Ok(MsgFunction::SetGuestSystemInfo)
            }
            bindings::NV_VGPU_MSG_FUNCTION_SET_REGISTRY => Ok(MsgFunction::SetRegistry),
            bindings::NV_VGPU_MSG_FUNCTION_UNLOADING_GUEST_DRIVER => {
                Ok(MsgFunction::UnloadingGuestDriver)
            }

            // Event codes
            bindings::NV_VGPU_MSG_EVENT_GSP_INIT_DONE => Ok(MsgFunction::GspInitDone),
            bindings::NV_VGPU_MSG_EVENT_GSP_LOCKDOWN_NOTICE => Ok(MsgFunction::GspLockdownNotice),
            bindings::NV_VGPU_MSG_EVENT_GSP_POST_NOCAT_RECORD => Ok(MsgFunction::GspPostNoCat),
            bindings::NV_VGPU_MSG_EVENT_MMU_FAULT_QUEUED => Ok(MsgFunction::MmuFaultQueued),
            bindings::NV_VGPU_MSG_EVENT_OS_ERROR_LOG => Ok(MsgFunction::OsErrorLog),
            bindings::NV_VGPU_MSG_EVENT_POST_EVENT => Ok(MsgFunction::PostEvent),
            bindings::NV_VGPU_MSG_EVENT_RC_TRIGGERED => Ok(MsgFunction::RcTriggered),
            bindings::NV_VGPU_MSG_EVENT_UCODE_LIBOS_PRINT => Ok(MsgFunction::UcodeLibOsPrint),
            _ => Err(EINVAL),
        }
    }
}

impl From<MsgFunction> for u32 {
    fn from(value: MsgFunction) -> Self {
        // CAST: `MsgFunction` is `repr(u32)` and can thus be cast losslessly.
        value as u32
    }
}

/// Struct containing the arguments required to pass a memory buffer to the GSP
/// for use during initialisation.
///
/// The GSP only understands 4K pages (GSP_PAGE_SIZE), so even if the kernel is
/// configured for a larger page size (e.g. 64K pages), we need to give
/// the GSP an array of 4K pages. Since we only create physically contiguous
/// buffers the math to calculate the addresses is simple.
///
/// The buffers must be a multiple of GSP_PAGE_SIZE.  GSP-RM also currently
/// ignores the @kind field for LOGINIT, LOGINTR, and LOGRM, but expects the
/// buffers to be physically contiguous anyway.
///
/// The memory allocated for the arguments must remain until the GSP sends the
/// init_done RPC.
#[repr(transparent)]
pub(crate) struct LibosMemoryRegionInitArgument {
    inner: bindings::LibosMemoryRegionInitArgument,
}

// SAFETY: Padding is explicit and does not contain uninitialized data.
unsafe impl AsBytes for LibosMemoryRegionInitArgument {}

// SAFETY: This struct only contains integer types for which all bit patterns
// are valid.
unsafe impl FromBytes for LibosMemoryRegionInitArgument {}

impl LibosMemoryRegionInitArgument {
    pub(crate) fn new<'a, A: AsBytes + FromBytes + KnownSize + ?Sized>(
        name: &'static str,
        obj: &'a Coherent<'_, A>,
    ) -> impl Init<Self> + 'a {
        /// Generates the `ID8` identifier required for some GSP objects.
        fn id8(name: &str) -> u64 {
            let mut bytes = [0u8; core::mem::size_of::<u64>()];

            for (c, b) in name.bytes().rev().zip(&mut bytes) {
                *b = c;
            }

            u64::from_ne_bytes(bytes)
        }

        let init_inner = init!(bindings::LibosMemoryRegionInitArgument {
            id8: id8(name),
            pa: obj.dma_address(),
            size: num::usize_as_u64(obj.size()),
            kind: num::u32_into_u8::<
                { bindings::LibosMemoryRegionKind_LIBOS_MEMORY_REGION_CONTIGUOUS },
            >(),
            loc: num::u32_into_u8::<
                { bindings::LibosMemoryRegionLoc_LIBOS_MEMORY_REGION_LOC_SYSMEM },
            >(),
            ..Zeroable::init_zeroed()
        });

        init!(LibosMemoryRegionInitArgument {
            inner <- init_inner,
        })
    }
}

/// The msgq version that the driver uses.
const MSGQ_VERSION_MAJOR: u16 = 2;
const MSGQ_VERSION_MINOR: u16 = 0;

/// The msgq TX header, which describes a queue to the GSP: the msgq version that the driver uses,
/// and the queue's geometry. The queue pointers are in BAR0 registers rather than in this header.
#[repr(transparent)]
pub(crate) struct MsgqTxHeader(bindings::msgqTxHeader);

impl MsgqTxHeader {
    /// Creates the msgq TX header of a queue of `msg_count` elements of `msg_size` bytes each,
    /// whose first element starts `entry_off` bytes into the `msgq_size`-byte queue.
    pub(crate) fn new(msgq_size: u32, msg_size: u32, msg_count: u32, entry_off: u32) -> Self {
        Self(bindings::msgqTxHeader {
            versionMajor: MSGQ_VERSION_MAJOR,
            versionMinor: MSGQ_VERSION_MINOR,
            size: msgq_size,
            msgSize: msg_size,
            msgCount: msg_count,
            entryOff: entry_off,
            reserved: [0; 3],
        })
    }
}

// SAFETY: Padding is explicit and does not contain uninitialized data.
unsafe impl AsBytes for MsgqTxHeader {}

bitfield! {
    struct MsgHeaderVersion(u32) {
        31:24 major;
        23:16 minor;
    }
}

impl MsgHeaderVersion {
    const MAJOR_TOT: u8 = 3;
    const MINOR_TOT: u8 = 0;

    fn new() -> Self {
        Self::zeroed()
            .with_major(Self::MAJOR_TOT)
            .with_minor(Self::MINOR_TOT)
    }
}

impl bindings::rpc_message_header_v {
    fn init(sequence: u32, cmd_size: usize, function: MsgFunction) -> impl Init<Self, Error> {
        type RpcMessageHeader = bindings::rpc_message_header_v;

        try_init!(RpcMessageHeader {
            header_version: MsgHeaderVersion::new().into(),
            signature: bindings::NV_VGPU_MSG_SIGNATURE_VALID,
            function: function.into(),
            sequence,
            length: size_of::<Self>()
                .checked_add(cmd_size)
                .ok_or(EOVERFLOW)
                .and_then(|v| v.try_into().map_err(|_| EINVAL))?,
            rpc_result: 0xffffffff,
            rpc_result_private: 0xffffffff,
            ..Zeroable::init_zeroed()
        })
    }
}

/// The headers that open an RPC queue element: the queue element header and the RPC header.
#[repr(C)]
pub(crate) struct GspMsgElement {
    element_header: QueueElementHeader,
    rpc: bindings::rpc_message_header_v,
}

// `AsBytes` below requires that no padding separates the two headers.
static_assert!(
    size_of::<GspMsgElement>()
        == size_of::<QueueElementHeader>() + size_of::<bindings::rpc_message_header_v>()
);

impl GspMsgElement {
    /// Creates the queue element header and the RPC header of a command with a `cmd_size`-byte
    /// payload and the RPC sequence number `rpc_seq`.
    pub(crate) fn init(
        rpc_seq: u32,
        cmd_size: usize,
        function: MsgFunction,
    ) -> impl Init<Self, Error> {
        type RpcMessageHeader = bindings::rpc_message_header_v;

        try_init!(GspMsgElement {
            element_header: QueueElementHeader::new(
                NvdmType::RmRpc,
                size_of::<RpcMessageHeader>()
                    .checked_add(cmd_size)
                    .ok_or(EOVERFLOW)?,
            )?,
            rpc <- RpcMessageHeader::init(rpc_seq, cmd_size, function),
        })
    }

    /// Returns the length of the payload that follows the RPC header, or `None` if the message
    /// length in the queue element header is shorter than the RPC header.
    pub(crate) fn payload_length(&self) -> Option<usize> {
        self.element_header
            .payload_len(size_of::<bindings::rpc_message_header_v>())
    }

    /// Returns the length of the whole element, both headers included.
    pub(crate) fn length(&self) -> usize {
        self.element_header.element_len()
    }

    // Returns the sequence number of the message.
    pub(crate) fn sequence(&self) -> u32 {
        self.rpc.sequence
    }

    // Returns the function of the message, if it is valid, or the invalid function number as an
    // error.
    pub(crate) fn function(&self) -> Result<MsgFunction, u32> {
        self.rpc.function.try_into().map_err(|_| self.rpc.function)
    }

    // Returns the number of elements (i.e. memory pages) used by this message.
    pub(crate) fn element_count(&self) -> u32 {
        self.element_header.element_count()
    }
}

// SAFETY: All fields are integer types or contain only integer types, with no
// uninitialized padding bytes.
unsafe impl AsBytes for GspMsgElement {}

// SAFETY: All fields are integer types for which all bit patterns are valid.
unsafe impl FromBytes for GspMsgElement {}

/// First word of every queue element: `"MCTP"` in ASCII.
const MCTP_MAGIC: u32 = 0x4D43_5450;

/// The queue element header that opens every queue element, whatever kind of message follows.
///
/// It holds an MCTP (Management Component Transport Protocol) header and an NVDM (NVIDIA
/// vendor-defined message) header. The NVDM type selects the message header that follows: the RPC
/// header or the GMC (GPU Management Controller) API header.
///
/// ```text
///     +------------------------------------+
///     | queue element header               |  QueueElementHeader: magic, element length, MCTP
///     |                                    |  header, NVDM header, message length
///     +------------------------------------+
///     | message header                     |  the RPC header or the GMC API header. The NVDM
///     +------------------------------------+  type selects between the two.
///     | payload                            |  command-specific data
///     +------------------------------------+
/// ```
#[repr(C)]
pub(crate) struct QueueElementHeader {
    magic: u32,
    /// Length of the whole element: the queue element header, the message header and the
    /// payload. Open RM calls it `mctpPayloadSize`.
    element_len: u32,
    mctp: MctpHeader,
    nvdm: NvdmHeader,
    /// Length of the message header and the payload, the queue element header excluded. Open RM
    /// calls it `nvdmPayloadSize`.
    message_len: u32,
    reserved: u32,
}

static_assert!(
    core::mem::offset_of!(QueueElementHeader, magic)
        == core::mem::offset_of!(bindings::GSP_MSG_QUEUE_ELEMENT, mctpMagic)
);
static_assert!(
    core::mem::offset_of!(QueueElementHeader, element_len)
        == core::mem::offset_of!(bindings::GSP_MSG_QUEUE_ELEMENT, mctpPayloadSize)
);
static_assert!(
    core::mem::offset_of!(QueueElementHeader, mctp)
        == core::mem::offset_of!(bindings::GSP_MSG_QUEUE_ELEMENT, mctpHeader)
);
static_assert!(
    core::mem::offset_of!(QueueElementHeader, nvdm)
        == core::mem::offset_of!(bindings::GSP_MSG_QUEUE_ELEMENT, nvdmHeader)
);
static_assert!(
    core::mem::offset_of!(QueueElementHeader, message_len)
        == core::mem::offset_of!(bindings::GSP_MSG_QUEUE_ELEMENT, __bindgen_anon_1)
            + core::mem::offset_of!(
                bindings::GSP_MSG_QUEUE_ELEMENT__bindgen_ty_1__bindgen_ty_2,
                nvdmPayloadSize
            )
);
static_assert!(
    core::mem::offset_of!(QueueElementHeader, reserved)
        == core::mem::offset_of!(bindings::GSP_MSG_QUEUE_ELEMENT, __bindgen_anon_1)
            + core::mem::offset_of!(
                bindings::GSP_MSG_QUEUE_ELEMENT__bindgen_ty_1__bindgen_ty_2,
                reserved
            )
);
// The bindings size the element for the encrypted tail. The unencrypted layout ends after the
// two length words, as `GSP_MSG_QUEUE_ELEMENT_SIZE_NO_ENCRYPTION` in Open RM defines it.
static_assert!(
    size_of::<QueueElementHeader>()
        == core::mem::offset_of!(bindings::GSP_MSG_QUEUE_ELEMENT, __bindgen_anon_1)
            + size_of::<bindings::GSP_MSG_QUEUE_ELEMENT__bindgen_ty_1__bindgen_ty_2>()
);

impl QueueElementHeader {
    /// Builds the queue element header of an element whose message header and payload together
    /// take `message_len` bytes.
    ///
    /// # Errors
    ///
    /// - `EOVERFLOW` if a length does not fit its 32-bit field.
    fn new(nvdm: NvdmType, message_len: usize) -> Result<Self> {
        Ok(Self {
            magic: MCTP_MAGIC,
            element_len: size_of::<Self>()
                .checked_add(message_len)
                .ok_or(EOVERFLOW)?
                .try_into()
                .map_err(|_| EOVERFLOW)?,
            mctp: MctpHeader::single_packet(),
            nvdm: NvdmHeader::new(nvdm),
            message_len: message_len.try_into().map_err(|_| EOVERFLOW)?,
            reserved: 0,
        })
    }

    /// Returns the length of the whole element, the queue element header included.
    pub(crate) fn element_len(&self) -> usize {
        num::u32_as_usize(self.element_len)
    }

    /// Returns the length of the payload that follows a message header of `message_header_len`
    /// bytes, or `None` if the message length in the queue element header is shorter than that
    /// header.
    fn payload_len(&self, message_header_len: usize) -> Option<usize> {
        num::u32_as_usize(self.message_len).checked_sub(message_header_len)
    }

    /// Returns the number of queue slots that this element occupies.
    pub(crate) fn element_count(&self) -> u32 {
        self.element_len
            .div_ceil(num::usize_into_u32::<GSP_PAGE_SIZE>())
    }

    /// Returns the NVDM type.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if the field holds no known NVDM type.
    pub(crate) fn nvdm_type(&self) -> Result<NvdmType> {
        self.nvdm.nvdm_type()
    }

    /// Validates the queue element header.
    ///
    /// Returns the first check that fails as a [`QueueElementHeaderError`].
    pub(crate) fn validate(&self) -> Result<(), QueueElementHeaderError> {
        if self.magic != MCTP_MAGIC {
            return Err(QueueElementHeaderError::BadMagic);
        }
        // The MCTP start-of-message and end-of-message bits are not checked. Every element carries
        // one whole message, because a large RPC is split into continuation records, not packets.
        if !self.mctp.has_expected_version() {
            return Err(QueueElementHeaderError::BadMctpVersion);
        }
        if !self.nvdm.has_nvidia_vendor() {
            return Err(QueueElementHeaderError::BadNvdmVendor);
        }

        // Under confidential compute, GSP-RM pads the element out to whole queue slots, so the
        // element may be longer than its queue element header and message together, but never
        // shorter.
        let length = self.element_len();
        let min_length = size_of::<Self>().saturating_add(num::u32_as_usize(self.message_len));
        if length < min_length || length > GSP_MSG_QUEUE_ELEMENT_SIZE_MAX {
            return Err(QueueElementHeaderError::BadLength);
        }

        Ok(())
    }
}

/// The check of [`QueueElementHeader::validate`] that a queue element header fails.
///
/// The receive path logs the variant when it poisons the queue, so that the log names the check
/// that failed.
#[derive(Debug, Clone, Copy)]
pub(crate) enum QueueElementHeaderError {
    /// The first word is not `"MCTP"`.
    BadMagic,
    /// The MCTP header carries a version other than the one that this driver uses.
    BadMctpVersion,
    /// The NVDM header names a vendor other than NVIDIA, or a message type other than
    /// vendor-defined.
    BadNvdmVendor,
    /// The element length is shorter than the queue element header and the message together, or
    /// above the maximum element size.
    BadLength,
}

// SAFETY: All fields are integer types or transparent wrappers over one, with no padding.
unsafe impl AsBytes for QueueElementHeader {}

// SAFETY: All fields are integer types for which all bit patterns are valid.
unsafe impl FromBytes for QueueElementHeader {}

/// Header of a GMC API message.
#[repr(C)]
#[derive(Zeroable)]
pub(crate) struct GmcApiHeader {
    /// Command id in the low three bytes, flags in the high byte.
    pub(crate) command: u32,
    /// Payload size in bytes.
    pub(crate) size: u32,
    /// Sequence number that GSP-RM copies from a request into its response.
    pub(crate) sequence: u64,
    /// In a request, the largest response that the sender accepts. In a response, the `NV_STATUS`.
    max_resp_or_status: u32,
    reserved: [u32; 5],
}

/// Bits of [`GmcApiHeader::command`] that hold the command id. The high byte holds flags.
const GMCAPI_COMMAND_ID_MASK: u32 = 0x00ff_ffff;

/// Flag bit of [`GmcApiHeader::command`] that GSP-RM sets on a response.
const GMCAPI_COMMAND_FLAGS_RESPONSE: u32 = 0x0100_0000;

/// GMC request that carries the system information and registry keys to GSP-RM. GSP-RM answers
/// it with the static GPU configuration once it has finished starting.
pub(crate) const GMCAPI_CMD_GSP_INIT: u32 = bindings::GMCAPI_COMMANDS_GMCAPI_CMD_GSP_INIT;

/// GMC event that requests the driver to run the generic falcon bootloader on the descriptor that
/// the event carries.
pub(crate) const GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER: u32 =
    bindings::GMCAPI_COMMANDS_GMCAPI_CMD_EXEC_GENERIC_BOOTLOADER;

/// GMC event that requests the driver to run a Heavy-Secured (HS) binary that GSP-RM has placed in
/// the framebuffer.
pub(crate) const GMCAPI_CMD_EXEC_HS_BINARY: u32 =
    bindings::GMCAPI_COMMANDS_GMCAPI_CMD_EXEC_HS_BINARY;

/// GMC request for GSP-RM to suspend. GSP-RM sends no response, and reports the completed
/// suspend in the GSP falcon's `MAILBOX0` instead.
pub(crate) const GMCAPI_CMD_GSP_SUSPEND: u32 = bindings::GMCAPI_COMMANDS_GMCAPI_CMD_GSP_SUSPEND;

static_assert!(size_of::<GmcApiHeader>() == size_of::<bindings::GMCAPI_HEADER>());
static_assert!(
    core::mem::offset_of!(GmcApiHeader, command)
        == core::mem::offset_of!(bindings::GMCAPI_HEADER, command)
);
static_assert!(
    core::mem::offset_of!(GmcApiHeader, size)
        == core::mem::offset_of!(bindings::GMCAPI_HEADER, size)
);
static_assert!(
    core::mem::offset_of!(GmcApiHeader, sequence)
        == core::mem::offset_of!(bindings::GMCAPI_HEADER, sequence)
);
static_assert!(
    core::mem::offset_of!(GmcApiHeader, max_resp_or_status)
        == core::mem::offset_of!(bindings::GMCAPI_HEADER, __bindgen_anon_1)
);
static_assert!(
    core::mem::offset_of!(GmcApiHeader, reserved)
        == core::mem::offset_of!(bindings::GMCAPI_HEADER, reserved)
);

impl GmcApiHeader {
    /// Returns the command id, without the flag byte.
    pub(crate) fn command_id(&self) -> u32 {
        self.command & GMCAPI_COMMAND_ID_MASK
    }

    /// Returns `true` if GSP-RM sent this header as a response rather than an event.
    fn is_response(&self) -> bool {
        self.command & GMCAPI_COMMAND_FLAGS_RESPONSE != 0
    }

    /// Returns the `NV_STATUS` that a response carries.
    ///
    /// The value is meaningful only when [`Self::is_response`] is `true`. In a request, the same
    /// word holds the largest response that the sender accepts.
    pub(crate) fn status(&self) -> u32 {
        self.max_resp_or_status
    }

    /// Returns `true` if this header answers the request with command id `command_id` and RPC
    /// sequence number `sequence`.
    pub(crate) fn is_response_to(&self, command_id: u32, sequence: u32) -> bool {
        self.is_response()
            && self.command_id() == command_id
            && self.sequence == u64::from(sequence)
    }
}

// SAFETY: All fields are integer types with no uninitialized padding bytes.
unsafe impl AsBytes for GmcApiHeader {}

// SAFETY: All fields are integer types for which all bit patterns are valid.
unsafe impl FromBytes for GmcApiHeader {}

/// The headers that open a GMC API queue element: the queue element header and the GMC API
/// header.
#[repr(C)]
pub(crate) struct GspGmcMsgElement {
    element_header: QueueElementHeader,
    pub(crate) gmc: GmcApiHeader,
}

// `AsBytes` below requires that no padding separates the two headers.
static_assert!(
    size_of::<GspGmcMsgElement>() == size_of::<QueueElementHeader>() + size_of::<GmcApiHeader>()
);

impl GspGmcMsgElement {
    /// Creates the queue element header and the GMC API header of a request that carries
    /// `payload_size` bytes of payload under the RPC sequence number `sequence`.
    ///
    /// `max_response_size` is the largest response that the sender accepts, and zero for a request
    /// that GSP-RM does not answer.
    ///
    /// # Errors
    ///
    /// - `EOVERFLOW` if a length does not fit its 32-bit field.
    pub(crate) fn init(
        command_id: u32,
        sequence: u32,
        payload_size: usize,
        max_response_size: u32,
    ) -> impl Init<Self, Error> {
        try_init!(GspGmcMsgElement {
            element_header: QueueElementHeader::new(
                NvdmType::GmcApi,
                size_of::<GmcApiHeader>()
                    .checked_add(payload_size)
                    .ok_or(EOVERFLOW)?,
            )?,
            gmc: GmcApiHeader {
                command: command_id,
                size: payload_size.try_into().map_err(|_| EOVERFLOW)?,
                sequence: u64::from(sequence),
                max_resp_or_status: max_response_size,
                reserved: [0; 5],
            },
        })
    }

    /// Returns the length of the payload that follows the GMC API header, or `None` if the
    /// message length in the queue element header is shorter than the GMC API header.
    pub(crate) fn payload_length(&self) -> Option<usize> {
        self.element_header.payload_len(size_of::<GmcApiHeader>())
    }

    /// Returns the length of the whole element, both headers included.
    pub(crate) fn length(&self) -> usize {
        self.element_header.element_len()
    }

    /// Returns the number of queue slots that this element occupies.
    pub(crate) fn element_count(&self) -> u32 {
        self.element_header.element_count()
    }
}

// SAFETY: All fields are integer types with no uninitialized padding bytes.
unsafe impl AsBytes for GspGmcMsgElement {}

// SAFETY: All fields are integer types for which all bit patterns are valid.
unsafe impl FromBytes for GspGmcMsgElement {}

/// First word of `GSP_ARGUMENTS_CACHED`: `"GSP "` in ASCII.
const GSP_ARGUMENTS_MAGIC_VALUE: u32 = 0x2050_5347;

/// Flag that requests GSP-RM to place its stack in DMEM.
const GSP_ARGUMENTS_FLAG_STACK_IN_DMEM: u64 = 0x02;

/// Arguments for GSP startup.
#[repr(transparent)]
#[derive(Zeroable)]
pub(crate) struct GspArgumentsCached {
    inner: bindings::GSP_ARGUMENTS_CACHED,
}

impl GspArgumentsCached {
    /// Creates the arguments for starting the GSP, with `cmdq` as its command queue and
    /// `state_monitor` as the buffer in which GSP-RM reports its own state.
    pub(crate) fn new<'a, 'b>(
        cmdq: &'a Cmdq<'b>,
        state_monitor: &'a Coherent<'b, [u8; GSP_PAGE_SIZE]>,
    ) -> impl Init<Self> + use<'a, 'b> {
        let init_inner = init!(bindings::GSP_ARGUMENTS_CACHED {
            magic: GSP_ARGUMENTS_MAGIC_VALUE,
            size: num::usize_into_u16::<{ size_of::<bindings::GSP_ARGUMENTS_CACHED>() }>(),
            flags: GSP_ARGUMENTS_FLAG_STACK_IN_DMEM,
            messageQueueInitArguments <- MessageQueueInitArguments::new(cmdq),
            rmStateMonitorBufferArgs: bindings::GSP_ARGUMENTS_CACHED__bindgen_ty_3 {
                pa: state_monitor.dma_address(),
                size: num::usize_as_u64(state_monitor.size()),
            },
            ..Zeroable::init_zeroed()
        });

        init!(GspArgumentsCached {
            inner <- init_inner,
        })
    }
}

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for GspArgumentsCached {}

/// On Turing and GA100, the entries in the `LibosMemoryRegionInitArgument`
/// must all be a multiple of GSP_PAGE_SIZE in size, so add padding to force it
/// to that size.
#[repr(C)]
#[derive(Zeroable)]
pub(crate) struct GspArgumentsPadded {
    pub(crate) inner: GspArgumentsCached,
    _padding: [u8; GSP_PAGE_SIZE - core::mem::size_of::<bindings::GSP_ARGUMENTS_CACHED>()],
}

impl GspArgumentsPadded {
    pub(crate) fn new<'a, 'b>(
        cmdq: &'a Cmdq<'b>,
        state_monitor: &'a Coherent<'b, [u8; GSP_PAGE_SIZE]>,
    ) -> impl Init<Self> + use<'a, 'b> {
        init!(GspArgumentsPadded {
            inner <- GspArgumentsCached::new(cmdq, state_monitor),
            ..Zeroable::init_zeroed()
        })
    }

    /// Records where `ucodes` is mapped.
    ///
    /// GSP-RM reads the arguments once, when it starts, so a write after that point has no effect.
    pub(crate) fn set_bindata(this: &Coherent<'_, Self>, ucodes: &UcodesImage<'_>) {
        io_write!(this, .inner.inner.bindataArgs.radix3, ucodes.radix3_dma_address());
        io_write!(this, .inner.inner.bindataArgs.size, num::usize_as_u64(ucodes.size()));
    }
}

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for GspArgumentsPadded {}

// SAFETY: This struct only contains integer types for which all bit patterns
// are valid.
unsafe impl FromBytes for GspArgumentsPadded {}

/// Init arguments for the message queue.
type MessageQueueInitArguments = bindings::MESSAGE_QUEUE_INIT_ARGUMENTS;

impl MessageQueueInitArguments {
    /// Creates a new init arguments structure for `cmdq`.
    fn new<'a, 'b>(cmdq: &'a Cmdq<'b>) -> impl Init<Self> + use<'a, 'b> {
        init!(MessageQueueInitArguments {
            sharedMemPhysAddr: cmdq.dma_addr,
            pageTableEntryCount: num::usize_into_u32::<{ Cmdq::NUM_PTES }>(),
            cmdQueueOffset: num::usize_as_u64(Cmdq::CMDQ_OFFSET),
            statQueueOffset: num::usize_as_u64(Cmdq::STATQ_OFFSET),

            queueElementHdrSize: num::usize_into_u32::<{ size_of::<QueueElementHeader>() }>(),
            queueElementSizeMin: num::usize_into_u32::<GSP_PAGE_SIZE>(),
            queueElementSizeMax: num::usize_into_u32::<GSP_MSG_QUEUE_ELEMENT_SIZE_MAX>(),

            // Both alignments are log2 values, which GSP-RM applies as `1 << n`.
            queueHeaderAlign: 4,
            queueElementAlign: num::usize_into_u32::<GSP_PAGE_SHIFT>(),

            ..Zeroable::init_zeroed()
        })
    }
}

#[repr(u32)]
pub(crate) enum GspDmaTarget {
    #[expect(dead_code)]
    LocalFb = bindings::GSP_DMA_TARGET_GSP_DMA_TARGET_LOCAL_FB,
    CoherentSystem = bindings::GSP_DMA_TARGET_GSP_DMA_TARGET_COHERENT_SYSTEM,
    NoncoherentSystem = bindings::GSP_DMA_TARGET_GSP_DMA_TARGET_NONCOHERENT_SYSTEM,
}

type GspAcrBootGspRmParams = bindings::GSP_ACR_BOOT_GSP_RM_PARAMS;

impl GspAcrBootGspRmParams {
    fn new(target: GspDmaTarget, wpr_meta_addr: u64) -> impl Init<Self> {
        let params = init!(Self {
            target: target as u32,
            gspRmDescSize: num::usize_into_u32::<{ size_of::<GspFwWprMeta>() }>(),
            gspRmDescOffset: wpr_meta_addr,
            bIsGspRmBoot: 1,
            wprCarveoutOffset: 0,
            wprCarveoutSize: 0,
            bInstInSysMode: 0,
            bIcuEnabled: 0,
            bScrubCbcSr: 0,
        });

        params
    }
}

type GspRmParams = bindings::GSP_RM_PARAMS;

impl GspRmParams {
    fn new(target: GspDmaTarget, libos_addr: u64) -> impl Init<Self> {
        let params = init!(Self {
            target: target as u32,
            reserved: 0,
            bootArgsOffset: libos_addr,
        });

        params
    }
}

pub(crate) type GspFmcBootParams = bindings::GSP_FMC_BOOT_PARAMS;

/// Magic value opening the ABI-stable `GSP_FMC_BOOT_PARAMS` header: `"FMC "` in ASCII.
const GSP_FMC_BOOT_PARAMS_MAGIC: u32 = 0x2043_4d46;

// SAFETY: Padding is explicit and will not contain uninitialized data.
unsafe impl AsBytes for GspFmcBootParams {}
// SAFETY: This struct only contains integer types for which all bit patterns are valid.
unsafe impl FromBytes for GspFmcBootParams {}

impl GspFmcBootParams {
    pub(crate) fn new(wpr_meta_addr: u64, libos_addr: u64) -> impl Init<Self> {
        let init = init!(Self {
            magic: GSP_FMC_BOOT_PARAMS_MAGIC,
            size: num::usize_into_u16::<{ size_of::<Self>() }>(),
            // Blackwell FSP obtains WPR info from other sources, so
            // wprCarveoutOffset and wprCarveoutSize are left zero.
            bootGspRmParams <- GspAcrBootGspRmParams::new(GspDmaTarget::CoherentSystem,
                wpr_meta_addr),
            gspRmParams <- GspRmParams::new(GspDmaTarget::NoncoherentSystem, libos_addr),
            ..Zeroable::init_zeroed()
        });

        init
    }
}
