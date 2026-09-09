// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use core::{
    ffi::FromBytesUntilNulError,
    ops::Range,
    str::Utf8Error, //
};

use kernel::{
    device,
    pci,
    prelude::*,
    transmute::AsBytes, //
};

use crate::{
    gpu::Chipset,
    gsp::{
        cmdq::Cmdq,
        fw::{
            self,
            commands::{
                GspInitRequest,
                GspInitResponse,
                GspInitResponseSchema,
                RegKey, //
            },
            GMCAPI_CMD_GSP_INIT,
            GMCAPI_CMD_GSP_SUSPEND, //
        },
        nvkv::{
            Decoder,
            Encodable,
            EncodedStream,
            Encoder,
            UnknownKeyPolicy, //
        },
    },
    sbuffer::SBufferIter,
    vgpu::VgpuState, //
};

/// The static GPU configuration, as decoded from the `GSP_INIT` reply.
pub(crate) struct GspStaticInfo {
    gpu_name: [u8; 64],
    /// BAR1 Page Directory Entry base address.
    pub(crate) bar1_pde_base: u64,
    /// Usable FB (VRAM) regions for driver memory allocation.
    pub(crate) usable_fb_regions: KVec<Range<u64>>,
    /// Exclusive end of the FB physical address space.
    pub(crate) total_fb_end: u64,
}

/// Error type for [`GspStaticInfo::gpu_name`].
#[derive(Debug)]
pub(crate) enum GpuNameError {
    /// The GPU name string does not contain a null terminator.
    NoNullTerminator(FromBytesUntilNulError),

    /// The GPU name string contains invalid UTF-8.
    #[expect(dead_code)]
    InvalidUtf8(Utf8Error),
}

impl GspStaticInfo {
    /// Returns the name of the GPU as a string.
    ///
    /// Returns an error if the string given by the GSP does not contain a null terminator or
    /// contains invalid UTF-8.
    pub(crate) fn gpu_name(&self) -> core::result::Result<&str, GpuNameError> {
        CStr::from_bytes_until_nul(&self.gpu_name)
            .map_err(GpuNameError::NoNullTerminator)?
            .to_str()
            .map_err(GpuNameError::InvalidUtf8)
    }
}

/// Registry entries the driver sends to GSP-RM on every boot.
///
/// `RMSecBusResetEnable` enables PCI secondary bus reset. `RMForcePcieConfigSave` makes GSP-RM
/// preserve PCI configuration registers across any PCI reset. `RMDevidCheckIgnore` lets GSP-RM
/// boot when the PCI device id is absent from its product name database.
const REGISTRY_ENTRIES: &[(&[u8], u32)] = &[
    (b"RMSecBusResetEnable\0", 1),
    (b"RMForcePcieConfigSave\0", 1),
    (b"RMDevidCheckIgnore\0", 1),
];

/// Builds the NVKV-encoded payload of a `GSP_INIT` request.
///
/// The payload carries the system information GSP-RM reads before it starts, and
/// [`REGISTRY_ENTRIES`] as `REGKEY_NAME` and `REGKEY_VALUE_U32` pairs. GSP-RM requires each name
/// to be followed by its value, which is the order [`RegKey`] declares them in.
///
/// # Errors
///
/// - `ENOMEM` if the registry list or the encoder buffer cannot be allocated.
pub(crate) fn build_gsp_init_payload(
    pdev: &pci::Device<device::Bound>,
    chipset: Chipset,
    vgpu_state: VgpuState,
) -> Result<EncodedStream> {
    let mut regkeys = KVVec::new();
    for &(name, value) in REGISTRY_ENTRIES {
        regkeys.push(RegKey::new(name, value), GFP_KERNEL)?;
    }
    if matches!(vgpu_state, VgpuState::Enabled { .. }) {
        regkeys.push(RegKey::new(b"RMSetSriovMode\0", 1), GFP_KERNEL)?;
    }

    let mut encoder = Encoder::new();
    GspInitRequest::new(pdev, chipset, regkeys).encode(&mut encoder)?;

    Ok(encoder.finish())
}

/// Largest `GSP_INIT` response the driver accepts.
const GSP_INIT_MAX_RESPONSE_SIZE: u32 = 48 * 1024;

/// Sends `GSP_INIT` and returns the static GPU configuration that its reply carries.
///
/// Each load-and-execute event that GSP-RM raises before the reply goes to `on_boot_event`.
///
/// `payload` is the stream from [`build_gsp_init_payload`].
///
/// # Errors
///
/// - `EIO` if GSP-RM reports a failure status, or if the reply is not a whole number of NVKV
///   words.
/// - `ETIMEDOUT` if neither the reply nor another element arrives within
///   [`Cmdq::RECEIVE_TIMEOUT`].
///
/// Errors from `on_boot_event` and from decoding the reply are propagated as-is.
pub(crate) fn gsp_init(
    cmdq: &Cmdq<'_>,
    payload: &[u64],
    mut on_boot_event: impl FnMut(u32, &[u8]) -> Result,
) -> Result<GspStaticInfo> {
    // Qualified because `zerocopy::IntoBytes` also gives `[T]` an `as_bytes`.
    let payload = AsBytes::as_bytes(payload);

    let sequence =
        cmdq.send_gmc_no_wait(GMCAPI_CMD_GSP_INIT, payload, GSP_INIT_MAX_RESPONSE_SIZE)?;

    loop {
        let reply = cmdq.receive_gmc_and_dispatch(
            Cmdq::RECEIVE_TIMEOUT,
            |header, payload_0, payload_1| {
                if header.is_response_to(GMCAPI_CMD_GSP_INIT, sequence) {
                    Some(decode_gsp_init_reply(
                        header.gmc.max_resp_or_status,
                        payload_0,
                        payload_1,
                    ))
                } else {
                    // A boot event. Keep waiting for the reply unless handling it failed.
                    match on_boot_event(header.gmc.command_id(), payload_0) {
                        Ok(()) => None,
                        Err(e) => Some(Err(e)),
                    }
                }
            },
        )?;

        if let Some(reply) = reply {
            return reply;
        }
    }
}

/// Decodes the `GSP_INIT` reply, whose `max_resp_or_status` field carries an `NV_STATUS`.
fn decode_gsp_init_reply(status: u32, payload_0: &[u8], payload_1: &[u8]) -> Result<GspStaticInfo> {
    if status != 0 {
        return Err(EIO);
    }

    decode_gsp_info(&nvkv_words(payload_0, payload_1)?)
}

/// Joins the two halves of a wrapped payload into the `u64` words an NVKV stream is made of.
///
/// # Errors
///
/// - `EIO` if the combined length is not a whole number of words.
/// - `ENOMEM` if the buffer cannot be allocated.
fn nvkv_words(payload_0: &[u8], payload_1: &[u8]) -> Result<KVVec<u64>> {
    let bytes = SBufferIter::new_reader([payload_0, payload_1]).flush_into_kvec(GFP_KERNEL)?;
    let words = bytes.chunks_exact(size_of::<u64>());
    if !words.remainder().is_empty() {
        return Err(EIO);
    }

    let mut out = KVVec::with_capacity(bytes.len() / size_of::<u64>(), GFP_KERNEL)?;
    for word in words {
        let word: [u8; size_of::<u64>()] = word.try_into().map_err(|_| EIO)?;
        out.push(u64::from_le_bytes(word), GFP_KERNEL)?;
    }

    Ok(out)
}

/// Decodes the static GPU configuration from an NVKV stream.
///
/// # Errors
///
/// - `EINVAL` if the stream is malformed or omits a required key.
/// - `ENOMEM` if the decoded regions cannot be allocated.
fn decode_gsp_info(words: &[u64]) -> Result<GspStaticInfo> {
    let decoder = Decoder::new(words, UnknownKeyPolicy::Ignore);
    let mut schema = GspInitResponseSchema::default();
    let decoded = KBox::try_init(decoder.decode(&mut schema)?, GFP_KERNEL)?;

    let mut gpu_name = [0u8; GspInitResponse::MAX_GPU_NAME_LEN];
    let name = decoded.gpu_name();
    gpu_name
        .get_mut(..name.len())
        .ok_or(EINVAL)?
        .copy_from_slice(name);

    let mut usable_fb_regions = KVec::new();
    for region in decoded.usable_fb_regions() {
        usable_fb_regions.push(region, GFP_KERNEL)?;
    }

    Ok(GspStaticInfo {
        gpu_name,
        bar1_pde_base: decoded.bar1_pde_base(),
        usable_fb_regions,
        total_fb_end: decoded.total_fb_end().ok_or(EINVAL)?,
    })
}

pub(crate) use fw::commands::PowerStateLevel;

/// Sends `GSP_SUSPEND`. GSP-RM does not answer it (see [`GMCAPI_CMD_GSP_SUSPEND`]).
///
/// # Errors
///
/// - `EMSGSIZE` if the request exceeds the maximum queue element size.
/// - `ETIMEDOUT` if queue space does not become available within the timeout.
/// - `EIO` if the element header is not properly aligned.
pub(crate) fn gsp_suspend(cmdq: &Cmdq<'_>, level: PowerStateLevel) -> Result {
    let params = fw::commands::GspSuspend::new(level);

    cmdq.send_gmc_no_wait(GMCAPI_CMD_GSP_SUSPEND, AsBytes::as_bytes(&params), 0)
        .map(|_| ())
}
