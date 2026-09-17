// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

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

pub(crate) use fw::commands::GspStaticInfo;

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

/// Decodes the `GSP_INIT` reply from its payload, which the ring may have split in two.
///
/// # Errors
///
/// - `EINVAL` if the payload is not a whole number of NVKV words, or if the stream is malformed
///   or omits a required key.
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
    let info = KBox::try_init(decoder.decode(&mut schema)?, GFP_KERNEL)?;

    Ok(KBox::into_inner(info))
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
