// SPDX-License-Identifier: GPL-2.0

// TODO: remove this once the code is fully functional
#![allow(dead_code)]

//! FSP (Firmware System Processor) interface for Blackwell+ GPUs.
//!
//! Blackwell uses a simplified firmware boot sequence: FMC → FSP → GSP.
//! Unlike Turing/Ampere/Ada, there is NO SEC2 (Security Engine 2) usage.
//! FSP handles secure boot directly using FMC firmware + Chain of Trust.

use kernel::device;
use kernel::dma::CoherentAllocation;
use kernel::prelude::*;
use kernel::ptr::Alignment;
use kernel::time::Delta;
use kernel::transmute::{AsBytes, FromBytes, FromBytesSized};

use crate::dma::DmaObject;
use crate::driver::Bar0;
use crate::falcon::fsp::Fsp as FspEngine;
use crate::falcon::Falcon;
use crate::fb;
use crate::gpu::Chipset;
use crate::regs;
use crate::util;

/// FSP Chain of Trust (COT) version for Blackwell.
/// GB202 uses version 2 (not 1 like GH100)
/// TODO: add a HAL so that both GB202 and GH100 work correctly! This is hardcoded for GB202.
const FSP_COT_VERSION: u16 = 2;

/// Size constraints for FSP security signatures.
const FSP_HASH_SIZE: usize = 48; // SHA-384 hash (12 x u32)
const FSP_PKEY_SIZE: usize = 97; // Public key size for GB202 (not 384!)
const FSP_SIG_SIZE: usize = 96; // Signature size for GB202 (not 384!)

/// FSP message timeout in milliseconds.
const FSP_MSG_TIMEOUT_MS: i64 = 2000;

/// FSP secure boot completion timeout in milliseconds.
const FSP_SECURE_BOOT_TIMEOUT_MS: i64 = 4000;

/// FSP boot completion status success value.
const FSP_BOOT_COMPLETE_STATUS_SUCCESS: u32 = 0x000000FF;

/// FSP error code: Invalid Chain of Trust payload structure.
const FSP_ERROR_INVALID_COT_PAYLOAD: u32 = 0x00D7;

/// FSP error code: Unrecognized microcode descriptor content.
const FSP_ERROR_UCODE_UNRECOGNIZED_DESCRIPTOR_P1: u32 = 0x0177;

/// Structure to hold FMC signatures without stack overflow.
/// Allocating this on heap prevents large stack frame allocations.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FmcSignatures {
    pub hash384: [u32; 12],    // SHA-384 hash (48 bytes)
    pub public_key: [u32; 96], // RSA public key (384 bytes)
    pub signature: [u32; 96],  // RSA signature (384 bytes)
}

impl Default for FmcSignatures {
    fn default() -> Self {
        Self {
            hash384: [0u32; 12],
            public_key: [0u32; 96],
            signature: [0u32; 96],
        }
    }
}

/// DMA target constants for GSP boot parameters.
#[allow(dead_code)]
const GSP_DMA_TARGET_LOCAL_FB: u32 = 0;
const GSP_DMA_TARGET_COHERENT_SYSTEM: u32 = 1;
const GSP_DMA_TARGET_NONCOHERENT_SYSTEM: u32 = 2;

/// MCTP (Management Component Transport Protocol) header values for FSP communication.
mod mctp {
    pub(super) const HEADER_SOM: u32 = 1; // Start of Message
    pub(super) const HEADER_EOM: u32 = 1; // End of Message
    pub(super) const HEADER_SEID: u32 = 0; // Source Endpoint ID
    pub(super) const HEADER_SEQ: u32 = 0; // Sequence number

    pub(super) const MSG_TYPE_VENDOR_PCI: u32 = 0x7e;
    pub(super) const VENDOR_ID_NV: u32 = 0x10de;
    pub(super) const NVDM_TYPE_COT: u32 = 0x14;
    pub(super) const NVDM_TYPE_FSP_RESPONSE: u32 = 0x15;
}

/// GSP FMC initialization parameters.
/// GSP_FMC_INIT_PARAMS structure.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct GspFmcInitParams {
    /// CC initialization "registry keys"
    regkeys: u32,
}

// SAFETY: GspFmcInitParams is a simple C struct with only primitive types
unsafe impl AsBytes for GspFmcInitParams {}
unsafe impl FromBytesSized for GspFmcInitParams {}

/// GSP ACR (Authenticated Code RAM) boot parameters.
/// GSP_ACR_BOOT_GSP_RM_PARAMS structure.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct GspAcrBootGspRmParams {
    /// Physical memory aperture through which gspRmDescPa is accessed
    target: u32,
    /// Size in bytes of the GSP-RM descriptor structure
    gsp_rm_desc_size: u32,
    /// Physical offset in the target aperture of the GSP-RM descriptor structure
    gsp_rm_desc_offset: u64,
    /// Physical offset in FB to set the start of the WPR containing GSP-RM
    wpr_carveout_offset: u64,
    /// Size in bytes of the WPR containing GSP-RM
    wpr_carveout_size: u32,
    /// Whether to boot GSP-RM or GSP-Proxy through ACR
    b_is_gsp_rm_boot: u32,
}

// SAFETY: GspAcrBootGspRmParams is a simple C struct with only primitive types
unsafe impl AsBytes for GspAcrBootGspRmParams {}
unsafe impl FromBytesSized for GspAcrBootGspRmParams {}

/// GSP RM boot parameters.
/// GSP_RM_PARAMS structure.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct GspRmParams {
    /// Physical memory aperture through which bootArgsOffset is accessed
    target: u32,
    /// Physical offset in the memory aperture that will be passed to GSP-RM
    boot_args_offset: u64,
}

// SAFETY: GspRmParams is a simple C struct with only primitive types
unsafe impl AsBytes for GspRmParams {}
unsafe impl FromBytesSized for GspRmParams {}

/// GSP SPDM (Security Protocol and Data Model) parameters.
/// GSP_SPDM_PARAMS structure.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct GspSpdmParams {
    /// Physical Memory Aperture through which all addresses are accessed
    target: u32,
    /// Physical offset in the memory aperture where SPDM payload is stored
    payload_buffer_offset: u64,
    /// Size of the above payload buffer
    payload_buffer_size: u32,
}

// SAFETY: GspSpdmParams is a simple C struct with only primitive types
unsafe impl AsBytes for GspSpdmParams {}
unsafe impl FromBytesSized for GspSpdmParams {}

/// Complete GSP FMC boot parameters structure.
/// This is what FSP expects to receive - NOT a raw libos address!
/// GSP_FMC_BOOT_PARAMS structure.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GspFmcBootParams {
    init_params: GspFmcInitParams,
    boot_gsp_rm_params: GspAcrBootGspRmParams,
    gsp_rm_params: GspRmParams,
    gsp_spdm_params: GspSpdmParams,
}

// SAFETY: GspFmcBootParams is a simple C struct containing only primitive types
// and other structs that are also AsBytes/FromBytesSized
unsafe impl AsBytes for GspFmcBootParams {}
unsafe impl FromBytesSized for GspFmcBootParams {}

/// NVDM (NVIDIA Device Management) COT (Chain of Trust) payload structure.
/// NVDM_PAYLOAD_COT structure with single u64 fields.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct NvdmPayloadCot {
    version: u16,               // offset 0x0, size 2
    size: u16,                  // offset 0x2, size 2
    gsp_fmc_sysmem_offset: u64, // offset 0x4, size 8
    frts_sysmem_offset: u64,    // offset 0xC, size 8
    frts_sysmem_size: u32,      // offset 0x14, size 4
    frts_vidmem_offset: u64,    // offset 0x18, size 8
    frts_vidmem_size: u32,      // offset 0x20, size 4
    // Authentication related fields
    hash384: [u32; 12],               // offset 0x24, size 48 (0x30)
    public_key: [u32; 96],            // offset 0x54, size 384 (0x180)
    signature: [u32; 96],             // offset 0x1D4, size 384 (0x180)
    gsp_boot_args_sysmem_offset: u64, // offset 0x354, size 8 - Single u64
}

/// FSP Command Response payload structure.
/// NVDM_PAYLOAD_COMMAND_RESPONSE structure.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct NvdmPayloadCommandResponse {
    task_id: u32,
    command_nvdm_type: u32,
    error_code: u32,
}

/// Complete FSP message structure with MCTP and NVDM headers.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct FspMessage {
    mctp_header: u32,
    nvdm_header: u32,
    cot: NvdmPayloadCot,
}

/// SAFETY: FspMessage is a packed C struct with only integral fields.
unsafe impl AsBytes for FspMessage {}

/// Complete FSP response structure with MCTP and NVDM headers.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct FspResponse {
    mctp_header: u32,
    nvdm_header: u32,
    response: NvdmPayloadCommandResponse,
}

/// SAFETY: FspResponse is a packed C struct with only integral fields.
unsafe impl FromBytesSized for FspResponse {}

// FspResponse implements FromBytes through FromBytesSized

/// Compile-time verification of NvdmPayloadCot structure offsets.
/// This ensures field alignment matches the expected hardware layout.
macro_rules! verify_offset {
    ($struct:ty, $field:ident, $expected:expr) => {
        const _: () = {
            use core::mem::offset_of;
            const ACTUAL: usize = offset_of!($struct, $field);
            const EXPECTED: usize = $expected;
            assert!(ACTUAL == EXPECTED, "Offset mismatch for field");
        };
    };
}

// Verify critical field offsets match expected layout
verify_offset!(NvdmPayloadCot, version, 0x0);
verify_offset!(NvdmPayloadCot, size, 0x2);
verify_offset!(NvdmPayloadCot, gsp_fmc_sysmem_offset, 0x4);
verify_offset!(NvdmPayloadCot, frts_sysmem_offset, 0xC);
verify_offset!(NvdmPayloadCot, frts_sysmem_size, 0x14);
verify_offset!(NvdmPayloadCot, frts_vidmem_offset, 0x18);
verify_offset!(NvdmPayloadCot, frts_vidmem_size, 0x20);
verify_offset!(NvdmPayloadCot, hash384, 0x24);
verify_offset!(NvdmPayloadCot, public_key, 0x54);
verify_offset!(NvdmPayloadCot, signature, 0x1D4);
verify_offset!(NvdmPayloadCot, gsp_boot_args_sysmem_offset, 0x354);

// Verify total structure size is exactly 860 bytes
const _: () = {
    use core::mem::size_of;
    const ACTUAL_SIZE: usize = size_of::<NvdmPayloadCot>();
    const EXPECTED_SIZE: usize = 860;
    assert!(ACTUAL_SIZE == EXPECTED_SIZE, "NvdmPayloadCot size mismatch");
};

/// FSP interface for Blackwell+ GPUs.
pub(crate) struct Fsp;

impl Fsp {
    /// Creates FMC boot parameters structure for FSP.
    ///
    /// This structure tells FSP how to boot GSP-RM with the correct memory layout.
    /// Setup for GSP initialization parameters.
    pub(crate) fn create_fmc_boot_params(
        dev: &device::Device<device::Bound>,
        wpr_meta_addr: u64,
        wpr_meta_size: u32,
        libos_addr: u64,
    ) -> Result<CoherentAllocation<GspFmcBootParams>> {
        let fmc_boot_params = CoherentAllocation::<GspFmcBootParams>::alloc_coherent(
            dev,
            1,
            GFP_KERNEL | __GFP_ZERO,
        )?;

        // Configure ACR boot parameters (WPR metadata location) using dma_write! macro
        kernel::dma_write!(
            fmc_boot_params[0].boot_gsp_rm_params.target = GSP_DMA_TARGET_COHERENT_SYSTEM
        )?;
        kernel::dma_write!(
            fmc_boot_params[0].boot_gsp_rm_params.gsp_rm_desc_offset = wpr_meta_addr
        )?;
        kernel::dma_write!(fmc_boot_params[0].boot_gsp_rm_params.gsp_rm_desc_size = wpr_meta_size)?;

        // CRITICAL: For Blackwell, WPR carveout fields must be ZERO!
        // These fields remain zero after allocation
        // FSP for Blackwell expects wpr_carveout_offset = 0 and wpr_carveout_size = 0
        // Unlike other architectures, Blackwell FSP gets WPR info from other sources

        kernel::dma_write!(fmc_boot_params[0].boot_gsp_rm_params.b_is_gsp_rm_boot = 1)?;

        // Configure RM parameters (libos location) using dma_write! macro
        kernel::dma_write!(
            fmc_boot_params[0].gsp_rm_params.target = GSP_DMA_TARGET_NONCOHERENT_SYSTEM
        )?;
        kernel::dma_write!(fmc_boot_params[0].gsp_rm_params.boot_args_offset = libos_addr)?;

        // Debug: Print actual field values being set
        dev_dbg!(
            dev,
            "=== FMC Boot Params Debug (addr={:#x}) ===\n",
            fmc_boot_params.dma_handle()
        );
        dev_dbg!(dev, "initParams.regkeys: {:#08x}\n", 0u32); // Always 0 for Nova
        dev_dbg!(
            dev,
            "bootGspRmParams.target: {}\n",
            GSP_DMA_TARGET_COHERENT_SYSTEM
        );
        dev_dbg!(
            dev,
            "bootGspRmParams.gspRmDescSize: {:#08x}\n",
            wpr_meta_size
        );
        dev_dbg!(
            dev,
            "bootGspRmParams.gspRmDescOffset: {:#016x}\n",
            wpr_meta_addr
        );
        dev_dbg!(dev, "bootGspRmParams.wprCarveoutOffset: {:#016x}\n", 0u64);
        dev_dbg!(dev, "bootGspRmParams.wprCarveoutSize: {:#08x}\n", 0u32);
        dev_dbg!(dev, "bootGspRmParams.bIsGspRmBoot: {}\n", 1u32);
        dev_dbg!(
            dev,
            "gspRmParams.target: {}\n",
            GSP_DMA_TARGET_NONCOHERENT_SYSTEM
        );
        dev_dbg!(dev, "gspRmParams.bootArgsOffset: {:#016x}\n", libos_addr);
        dev_dbg!(dev, "gspSpdmParams.target: {}\n", 0u32);
        dev_dbg!(dev, "gspSpdmParams.payloadBufferOffset: {:#016x}\n", 0u64);
        dev_dbg!(dev, "gspSpdmParams.payloadBufferSize: {:#08x}\n", 0u32);

        Ok(fmc_boot_params)
    }

    /// Extract FMC firmware signatures for Chain of Trust verification.
    ///
    /// Extracts real cryptographic signatures from FMC ELF32 firmware sections.
    /// Extract FMC firmware signatures for Chain of Trust verification.
    ///
    /// Returns signatures in a heap-allocated structure to prevent stack overflow.
    pub(crate) fn extract_fmc_signatures_static(
        dev: &device::Device<device::Bound>,
        fmc_fw_data: &[u8],
    ) -> Result<KBox<FmcSignatures>> {
        dev_dbg!(dev, "FMC firmware size: {} bytes\n", fmc_fw_data.len());

        // Extract hash section (SHA-384)
        let hash_section = crate::firmware::elf_section(fmc_fw_data, "hash")
            .ok_or(EINVAL)
            .inspect_err(|_| dev_err!(dev, "FMC firmware missing 'hash' section\n"))?;

        // Extract public key section (RSA public key)
        let pkey_section = crate::firmware::elf_section(fmc_fw_data, "publickey")
            .ok_or(EINVAL)
            .inspect_err(|_| dev_err!(dev, "FMC firmware missing 'publickey' section\n"))?;

        // Extract signature section (RSA signature)
        let sig_section = crate::firmware::elf_section(fmc_fw_data, "signature")
            .ok_or(EINVAL)
            .inspect_err(|_| dev_err!(dev, "FMC firmware missing 'signature' section\n"))?;

        dev_dbg!(
            dev,
            "FMC ELF sections: hash={} bytes, pkey={} bytes, sig={} bytes\n",
            hash_section.len(),
            pkey_section.len(),
            sig_section.len()
        );

        // Validate section sizes - hash must be exactly 48 bytes, but pkey/sig can be smaller
        if hash_section.len() != FSP_HASH_SIZE {
            dev_err!(
                dev,
                "FMC hash section size {} != expected {}\n",
                hash_section.len(),
                FSP_HASH_SIZE
            );
            return Err(EINVAL);
        }

        // Public key and signature can be smaller than the fixed array sizes
        if pkey_section.len() > FSP_PKEY_SIZE {
            dev_err!(
                dev,
                "FMC publickey section size {} > maximum {}\n",
                pkey_section.len(),
                FSP_PKEY_SIZE
            );
            return Err(EINVAL);
        }

        if sig_section.len() > FSP_SIG_SIZE {
            dev_err!(
                dev,
                "FMC signature section size {} > maximum {}\n",
                sig_section.len(),
                FSP_SIG_SIZE
            );
            return Err(EINVAL);
        }

        // STACK OVERFLOW FIX: Allocate large signature arrays on heap instead of stack
        // These arrays are 48 + 384 + 384 = 816 bytes and were causing stack overflow
        let mut hash_box = KBox::new([0u32; 12], GFP_KERNEL)?;
        let mut pkey_box = KBox::new([0u32; 96], GFP_KERNEL)?;
        let mut sig_box = KBox::new([0u32; 96], GFP_KERNEL)?;

        // Copy hash section directly as bytes (48 bytes exactly)
        let hash_bytes = hash_box.as_bytes_mut();
        hash_bytes[..hash_section.len().min(48)]
            .copy_from_slice(&hash_section[..hash_section.len().min(48)]);

        // Copy public key section directly as bytes (up to 384 bytes, zero-padded)
        let pkey_bytes = pkey_box.as_bytes_mut();
        pkey_bytes[..pkey_section.len().min(384)]
            .copy_from_slice(&pkey_section[..pkey_section.len().min(384)]);

        // Copy signature section directly as bytes (up to 384 bytes, zero-padded)
        let sig_bytes = sig_box.as_bytes_mut();
        sig_bytes[..sig_section.len().min(384)]
            .copy_from_slice(&sig_section[..sig_section.len().min(384)]);

        // Construct FmcSignatures directly from heap arrays to avoid stack copies
        let signatures = KBox::new(
            FmcSignatures {
                hash384: *hash_box,
                public_key: *pkey_box,
                signature: *sig_box,
            },
            GFP_KERNEL,
        )?;

        Ok(signatures)
    }

    /// Poll FSP for incoming data.
    fn poll_fsp(dev: &device::Device<device::Bound>, bar: &Bar0) -> u32 {
        let head = regs::NV_PFSP_MSGQ_HEAD::read(bar).address();
        let tail = regs::NV_PFSP_MSGQ_TAIL::read(bar).address();

        // Removed frequent debug output to prevent spam - only log issues
        if head == tail {
            return 0;
        }

        // Read tail register again
        let tail_fresh = regs::NV_PFSP_MSGQ_TAIL::read(bar).address();

        if tail_fresh < head {
            dev_err!(
                dev,
                "FSP MSGQ invalid state: tail_fresh={:#x} < head={:#x}\n",
                tail_fresh,
                head
            );
            return 0;
        }

        // TAIL points at last DWORD written - use checked arithmetic
        let diff = tail_fresh - head; // This is safe now since tail_fresh >= head
        match diff.checked_add(4) {
            Some(size) => {
                // Only log when data is actually detected
                if size > 0 {
                    dev_dbg!(dev, "FSP MSGQ poll: detected {} bytes available\n", size);
                }
                size
            }
            None => {
                dev_err!(
                    dev,
                    "FSP MSGQ size overflow: tail_fresh={:#x}, head={:#x}, diff={:#x}\n",
                    tail_fresh,
                    head,
                    diff
                );
                0
            }
        }
    }

    /// Wait for FSP to have data available and return the packet size.
    /// Returns the packet size.
    fn wait_fsp(dev: &device::Device<device::Bound>, bar: &Bar0) -> Result<u32> {
        dev_dbg!(dev, "FSP waiting for response\n");

        let initial_head = regs::NV_PFSP_MSGQ_HEAD::read(bar).address();
        let initial_tail = regs::NV_PFSP_MSGQ_TAIL::read(bar).address();
        dev_dbg!(
            dev,
            "FSP MSGQ initial state: head={:#x}, tail={:#x}\n",
            initial_head,
            initial_tail
        );

        match util::wait_on(Delta::from_millis(FSP_MSG_TIMEOUT_MS), || {
            let packet_size = Self::poll_fsp(dev, bar);
            if packet_size > 0 {
                dev_dbg!(dev, "FSP response detected, packet_size={}\n", packet_size);
                Some(packet_size)
            } else {
                None
            }
        }) {
            Ok(packet_size) => Ok(packet_size),
            Err(_) => {
                let final_head = regs::NV_PFSP_MSGQ_HEAD::read(bar).address();
                let final_tail = regs::NV_PFSP_MSGQ_TAIL::read(bar).address();
                dev_err!(
                    dev,
                    "FSP wait timeout - final MSGQ state: head={:#x}, tail={:#x}\n",
                    final_head,
                    final_tail
                );
                Err(ETIMEDOUT)
            }
        }
    }

    /// Send message to FSP
    /// Returns the packet size.
    fn send_fsp(
        dev: &device::Device<device::Bound>,
        bar: &Bar0,
        fsp_falcon: &Falcon<FspEngine>,
        packet: &[u8],
    ) -> Result<()> {
        let packet_size = packet.len();

        if packet_size % 4 != 0 {
            dev_err!(dev, "FSP packet size must be multiple of 4\n");
            return Err(EINVAL);
        }

        // Ensure any previously sent message has been consumed
        let timeout = Delta::from_millis(FSP_MSG_TIMEOUT_MS);
        util::wait_on(timeout, || {
            let head = regs::NV_PFSP_QUEUE_HEAD::read(bar).address();
            let tail = regs::NV_PFSP_QUEUE_TAIL::read(bar).address();

            if tail == head {
                Some(())
            } else {
                None
            }
        })
        .map_err(|_| {
            dev_err!(dev, "FSP send timeout waiting for queue clear\n");
            ETIMEDOUT
        })?;

        // Write message to EMEM using falcon PIO write
        fsp_falcon.write_emem(bar, 0, packet)?;

        // Update queue pointers - TAIL points at last DWORD written
        regs::NV_PFSP_QUEUE_TAIL::default()
            .set_address(packet_size as u32 - 4)
            .write(bar);
        regs::NV_PFSP_QUEUE_HEAD::default()
            .set_address(0)
            .write(bar);

        dev_dbg!(
            dev,
            "FSP sent {} bytes to EMEM, TAIL={}\n",
            packet_size,
            packet_size - 4
        );

        Ok(())
    }

    /// Receive message from FSP via falcon EMEM.
    /// Returns the packet data.
    fn recv_fsp(
        dev: &device::Device<device::Bound>,
        bar: &Bar0,
        fsp_falcon: &Falcon<FspEngine>,
        packet_size: u32,
        max_size: usize,
    ) -> Result<KVec<u8>> {
        let packet_size = packet_size as usize;

        if packet_size == 0 || packet_size % 4 != 0 || packet_size > max_size {
            dev_err!(
                dev,
                "Invalid FSP packet size: {} (max: {})\n",
                packet_size,
                max_size
            );
            return Err(EINVAL);
        }

        // Read response from EMEM using falcon PIO read
        let mut packet = KVec::with_capacity(packet_size, GFP_KERNEL)?;
        packet.resize(packet_size, 0u8, GFP_KERNEL)?;
        fsp_falcon.read_emem(bar, 0, &mut packet)?;

        // Reset message queue pointers after reading
        regs::NV_PFSP_MSGQ_TAIL::default().set_address(0).write(bar);
        regs::NV_PFSP_MSGQ_HEAD::default().set_address(0).write(bar);

        dev_dbg!(dev, "FSP received {} bytes from EMEM\n", packet_size);
        Ok(packet)
    }

    /// Send message to FSP and wait for response.
    fn send_sync_fsp(
        dev: &device::Device<device::Bound>,
        bar: &Bar0,
        fsp_falcon: &Falcon<FspEngine>,
        nvdm_type: u32,
        packet: &[u8],
    ) -> Result<()> {
        Self::send_fsp(dev, bar, fsp_falcon, packet)?;
        let packet_size = Self::wait_fsp(dev, bar)?;

        let response_data =
            Self::recv_fsp(dev, bar, fsp_falcon, packet_size, size_of::<FspResponse>())?;

        if response_data.len() < size_of::<FspResponse>() {
            dev_err!(dev, "FSP response too small: {}\n", response_data.len());
            return Err(EIO);
        }

        // Parse response
        let response = FspResponse::from_bytes(&response_data).ok_or(EIO)?;

        // Copy packed struct fields to avoid alignment issues
        let mctp_header = response.mctp_header;
        let nvdm_header = response.nvdm_header;
        let command_nvdm_type = response.response.command_nvdm_type;
        let error_code = response.response.error_code;

        // Validate MCTP header
        let mctp_som = (mctp_header >> 31) & 1;
        let mctp_eom = (mctp_header >> 30) & 1;
        if mctp_som != 1 || mctp_eom != 1 {
            dev_err!(
                dev,
                "Unexpected MCTP header in FSP reply: {:#x}\n",
                mctp_header
            );
            return Err(EIO);
        }

        // Validate NVDM header
        let nvdm_msg_type = nvdm_header & 0x7f;
        let nvdm_vendor_id = (nvdm_header >> 8) & 0xffff;
        let nvdm_type_resp = (nvdm_header >> 24) & 0xff;

        if nvdm_msg_type != mctp::MSG_TYPE_VENDOR_PCI
            || nvdm_vendor_id != mctp::VENDOR_ID_NV
            || nvdm_type_resp != mctp::NVDM_TYPE_FSP_RESPONSE
        {
            dev_err!(
                dev,
                "Unexpected NVDM header in FSP reply: {:#x}\n",
                nvdm_header
            );
            return Err(EIO);
        }

        // Check command type matches
        if command_nvdm_type != nvdm_type {
            dev_err!(
                dev,
                "Expected NVDM type {:#x} in reply, got {:#x}\n",
                nvdm_type,
                command_nvdm_type
            );
            return Err(EIO);
        }

        if error_code != 0 {
            let error_message = match error_code {
                FSP_ERROR_INVALID_COT_PAYLOAD => "INVALID_COT_PAYLOAD",
                FSP_ERROR_UCODE_UNRECOGNIZED_DESCRIPTOR_P1 => "UCODE_UNRECOGNIZED_DESCRIPTOR_P1",
                _ => "Unknown FSP error",
            };

            dev_err!(
                dev,
                "NVDM command {:#x} failed with error {:#x} ({})\n",
                nvdm_type,
                error_code,
                error_message
            );
            return Err(EIO);
        }

        dev_dbg!(dev, "FSP command {:#x} completed successfully\n", nvdm_type);
        Ok(())
    }

    /// Wait for FSP secure boot completion
    pub(crate) fn wait_secure_boot(
        dev: &device::Device<device::Bound>,
        bar: &Bar0,
        arch: crate::gpu::Architecture,
    ) -> Result<()> {
        let timeout = Delta::from_millis(FSP_SECURE_BOOT_TIMEOUT_MS);

        // Check if this architecture supports FSP thermal scratch register
        let initial_status = regs::read_fsp_boot_complete_status(bar, arch).inspect_err(|_| {
            dev_err!(
                dev,
                "FSP thermal scratch register not supported for architecture {:?}\n",
                arch
            )
        })?;
        dev_dbg!(
            dev,
            "FSP initial I2CS scratch register status: {:#x}\n",
            initial_status
        );

        util::wait_on(timeout, || {
            let status = regs::read_fsp_boot_complete_status(bar, arch).ok()?;
            dev_dbg!(
                dev,
                "FSP I2CS scratch register status: {:#x} (expected: {:#x})\n",
                status,
                FSP_BOOT_COMPLETE_STATUS_SUCCESS
            );
            if status == FSP_BOOT_COMPLETE_STATUS_SUCCESS {
                Some(())
            } else {
                None
            }
        })
        .map_err(|_| {
            let final_status = regs::read_fsp_boot_complete_status(bar, arch).unwrap_or(0xDEADBEEF);
            dev_err!(
                dev,
                "FSP secure boot completion timeout - final status: {:#x}\n",
                final_status
            );
            ETIMEDOUT
        })
    }

    /// Boot GSP FMC with pre-extracted signatures.
    ///
    /// This version takes pre-extracted signatures and FMC image data (not full ELF).
    /// Used when signatures are extracted separately from the full ELF file.
    pub(crate) fn boot_gsp_fmc_with_signatures(
        dev: &device::Device<device::Bound>,
        bar: &Bar0,
        chipset: Chipset,
        fmc_image_fw: &DmaObject, // Contains only the image section
        fmc_boot_params: &CoherentAllocation<GspFmcBootParams>,
        rsvd_size: u64,
        resume: bool,
        fsp_falcon: &Falcon<FspEngine>,
        signatures: &FmcSignatures,
    ) -> Result<()> {
        dev_dbg!(dev, "Starting FSP boot sequence for {}\n", chipset);

        // Build FSP Chain of Trust message
        let fmc_addr = fmc_image_fw.dma_handle(); // Now points to image data only
        let fmc_boot_params_addr = fmc_boot_params.dma_handle();

        // FRTS calculation: ALIGN(rsvd_size, 0x200000)
        // CRITICAL: frts_offset is a SIZE from the END of FB, not an absolute offset!
        // FSP calculates FRTS location as: FB_END - frts_offset = FRTS_location
        let frts_offset = if !resume {
            let mut final_rsvd_size = if chipset.needs_large_reserved_mem() {
                0x220000 // heap_size_non_wpr for Hopper/Blackwell+
            } else {
                rsvd_size
            };

            // Add PMU reserved size
            final_rsvd_size += fb::calc_pmu_reserved_size();

            Alignment::new(0x200000)
                .align_up(final_rsvd_size)
                .unwrap_or(final_rsvd_size)
        } else {
            0
        };
        let frts_size = if !resume { 0x100000 } else { 0 }; // 1MB FRTS size

        // Allocate FspMessage on heap (860 bytes).
        let msg = KBox::new(
            FspMessage {
                mctp_header: (mctp::HEADER_SOM << 31)
                    | (mctp::HEADER_EOM << 30)
                    | (mctp::HEADER_SEID << 16)
                    | (mctp::HEADER_SEQ << 28),

                nvdm_header: (mctp::MSG_TYPE_VENDOR_PCI)
                    | (mctp::VENDOR_ID_NV << 8)
                    | (mctp::NVDM_TYPE_COT << 24),

                cot: NvdmPayloadCot {
                    version: FSP_COT_VERSION,
                    size: size_of::<NvdmPayloadCot>() as u16,
                    gsp_fmc_sysmem_offset: fmc_addr,
                    frts_sysmem_offset: 0,
                    frts_sysmem_size: 0,
                    frts_vidmem_offset: frts_offset,
                    frts_vidmem_size: frts_size,
                    hash384: signatures.hash384,
                    public_key: signatures.public_key,
                    signature: signatures.signature,
                    gsp_boot_args_sysmem_offset: fmc_boot_params_addr,
                },
            },
            GFP_KERNEL,
        )?;

        // Convert message to bytes for sending
        let msg_bytes = msg.as_bytes();

        dev_dbg!(
            dev,
            "=== FSP Message Structure Debug ====================\n"
        );
        dev_dbg!(dev, "FspMessage total size: {} bytes\n", msg_bytes.len());

        // Copy packed field values to avoid alignment issues
        let mctp_header = msg.mctp_header;
        let nvdm_header = msg.nvdm_header;
        let cot_version = msg.cot.version;
        let cot_size = msg.cot.size;
        let gsp_fmc_offset = msg.cot.gsp_fmc_sysmem_offset;
        let frts_sysmem_offset = msg.cot.frts_sysmem_offset;
        let frts_sysmem_size = msg.cot.frts_sysmem_size;
        let frts_vidmem_offset = msg.cot.frts_vidmem_offset;
        let frts_vidmem_size = msg.cot.frts_vidmem_size;
        let gsp_boot_args_offset = msg.cot.gsp_boot_args_sysmem_offset;

        dev_dbg!(dev, "MCTP header: {:#x}\n", mctp_header);
        dev_dbg!(dev, "NVDM header: {:#x}\n", nvdm_header);
        dev_dbg!(dev, "COT payload:\n");
        dev_dbg!(dev, "  version: {}\n", cot_version);
        dev_dbg!(dev, "  size: {}\n", cot_size);
        dev_dbg!(dev, "  gspFmcSysmemOffset: {:#x}\n", gsp_fmc_offset);
        dev_dbg!(dev, "  frtsSysmemOffset: {:#x}\n", frts_sysmem_offset);
        dev_dbg!(dev, "  frtsSysmemSize: {:#x}\n", frts_sysmem_size);
        dev_dbg!(dev, "  frtsVidmemOffset: {:#x}\n", frts_vidmem_offset);
        dev_dbg!(dev, "  frtsVidmemSize: {:#x}\n", frts_vidmem_size);
        dev_dbg!(
            dev,
            "  gspBootArgsSysmemOffset: {:#x}\n",
            gsp_boot_args_offset
        );

        dev_dbg!(
            dev,
            "Sending {} byte FspMessage structure (with headers) to FSP falcon EMEM\n",
            msg_bytes.len()
        );

        // Send COT message to FSP and wait for synchronous response
        Self::send_sync_fsp(dev, bar, fsp_falcon, mctp::NVDM_TYPE_COT, msg_bytes)?;

        Ok(())
    }
}
