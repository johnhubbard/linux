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
                GspInitResponseSchema, //
            },
            GspGmcMsgElement,
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

/// Builds the NVKV-encoded payload of a `GSP_INIT` request for `pdev`.
///
/// # Errors
///
/// - `ENOMEM` if the request or the encoder buffer cannot be allocated.
pub(crate) fn build_gsp_init_payload(
    pdev: &pci::Device<device::Bound>,
    chipset: Chipset,
    vgpu_state: VgpuState,
) -> Result<EncodedStream> {
    let mut encoder = Encoder::new();
    GspInitRequest::new(pdev, chipset, vgpu_state)?.encode(&mut encoder)?;

    Ok(encoder.finish())
}

/// Largest `GSP_INIT` response that the driver accepts.
const GSP_INIT_MAX_RESPONSE_SIZE: u32 = 48 * 1024;

/// Sends `GSP_INIT` and returns the static GPU configuration that its reply carries.
///
/// Every GMC (GPU Management Controller) element that arrives before the reply is passed to
/// `on_unsolicited_element` with the headers that open it and its payload, as two slices because
/// the ring may wrap. The load-and-execute events that GSP-RM raises while it starts arrive this
/// way.
///
/// `payload` is the stream from [`build_gsp_init_payload`].
///
/// # Errors
///
/// - `EIO` if GSP-RM reports a failure status.
/// - `ETIMEDOUT` if the reply does not arrive within [`Cmdq::RECEIVE_TIMEOUT`] of the send,
///   however many events arrive while waiting.
///
/// Errors from `on_unsolicited_element` and from decoding the reply are propagated as-is.
pub(crate) fn gsp_init(
    cmdq: &Cmdq<'_>,
    payload: &[u64],
    on_unsolicited_element: impl FnMut(&GspGmcMsgElement, &[u8], &[u8]) -> Result,
) -> Result<GspStaticInfo> {
    // Qualified because `zerocopy::IntoBytes` also gives `[T]` an `as_bytes`.
    let payload = AsBytes::as_bytes(payload);

    cmdq.send_gmc_no_wait(GMCAPI_CMD_GSP_INIT, payload, GSP_INIT_MAX_RESPONSE_SIZE)?;

    cmdq.await_gmc_response(
        GMCAPI_CMD_GSP_INIT,
        on_unsolicited_element,
        decode_gsp_init_reply,
    )
}

/// Decodes the `GSP_INIT` reply from its payload, which the ring may have split in two, into the
/// static configuration type that the boot sequence returns.
///
/// # Errors
///
/// - `EINVAL` if the payload is not a whole number of NVKV words, if the stream is malformed or
///   omits a required key, or if GSP-RM reported no framebuffer region.
/// - `ENOMEM` if the words or the decoded regions cannot be allocated.
fn decode_gsp_init_reply(payload_0: &[u8], payload_1: &[u8]) -> Result<GspStaticInfo> {
    const WORD_SIZE: usize = size_of::<u64>();

    let len = payload_0.len() + payload_1.len();
    if len % WORD_SIZE != 0 {
        return Err(EINVAL);
    }

    let mut words = KVVec::with_capacity(len / WORD_SIZE, GFP_KERNEL)?;
    let mut bytes = SBufferIter::new_reader([payload_0, payload_1]);
    for _ in 0..len / WORD_SIZE {
        let mut word = [0u8; WORD_SIZE];
        bytes.read_exact(&mut word)?;
        words.push(u64::from_le_bytes(word), GFP_KERNEL)?;
    }

    let decoder = Decoder::new(&words, UnknownKeyPolicy::Ignore);
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

/// Sends `GSP_SUSPEND`, which GSP-RM does not answer (see [`GMCAPI_CMD_GSP_SUSPEND`]).
///
/// # Errors
///
/// Errors from [`Cmdq::send_gmc_no_wait`] are propagated as-is.
pub(crate) fn gsp_suspend(cmdq: &Cmdq<'_>, level: PowerStateLevel) -> Result {
    let params = fw::commands::GspSuspend::new(level);

    cmdq.send_gmc_no_wait(GMCAPI_CMD_GSP_SUSPEND, AsBytes::as_bytes(&params), 0)
}
