// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

use core::{
    array,
    convert::Infallible,
    ffi::FromBytesUntilNulError,
    ops::Range,
    str::Utf8Error, //
};

use kernel::{
    device,
    pci,
    prelude::*,
    transmute::{
        AsBytes,
        FromBytes, //
    }, //
};

use crate::{
    gpu::Chipset,
    gsp::{
        cmdq::{
            Cmdq,
            CommandToGsp,
            MessageFromGsp,
            NoReply, //
        },
        fw::{
            self,
            commands::{
                GspInitRequest,
                GspInitResponse,
                GspInitResponseSchema, //
            },
            GspGmcMsgElement,
            MsgFunction,
            GMCAPI_CMD_GSP_INIT, //
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

/// The `GspSetSystemInfo` command.
pub(crate) struct SetSystemInfo<'a> {
    pdev: &'a pci::Device<device::Bound>,
    chipset: Chipset,
}

impl<'a> SetSystemInfo<'a> {
    /// Creates a new `GspSetSystemInfo` command using the parameters of `pdev`.
    pub(crate) fn new(pdev: &'a pci::Device<device::Bound>, chipset: Chipset) -> Self {
        Self { pdev, chipset }
    }
}

impl<'a> CommandToGsp for SetSystemInfo<'a> {
    const FUNCTION: MsgFunction = MsgFunction::GspSetSystemInfo;
    type Command = fw::commands::GspSetSystemInfo;
    type Reply = NoReply;
    type InitError = Error;

    fn init(&self) -> impl Init<Self::Command, Self::InitError> {
        Self::Command::init(self.pdev, self.chipset)
    }
}

struct RegistryEntry {
    key: &'static str,
    value: u32,
}

/// The `SetRegistry` command.
pub(crate) struct SetRegistry {
    entries: KVec<RegistryEntry>,
}

impl SetRegistry {
    /// Creates a new `SetRegistry` command, using a set of hardcoded entries.
    pub(crate) fn new(vgpu_state: VgpuState) -> Result<Self> {
        let mut entries = KVec::new();

        // RMSecBusResetEnable - enables PCI secondary bus reset
        entries.push(
            RegistryEntry {
                key: "RMSecBusResetEnable",
                value: 1,
            },
            GFP_KERNEL,
        )?;

        // RMForcePcieConfigSave - forces GSP-RM to preserve PCI configuration registers on
        // any PCI reset.
        entries.push(
            RegistryEntry {
                key: "RMForcePcieConfigSave",
                value: 1,
            },
            GFP_KERNEL,
        )?;

        // RMDevidCheckIgnore - allows GSP-RM to boot even if the PCI dev ID is not found
        // in the internal product name database.
        entries.push(
            RegistryEntry {
                key: "RMDevidCheckIgnore",
                value: 1,
            },
            GFP_KERNEL,
        )?;

        if matches!(vgpu_state, VgpuState::Enabled { .. }) {
            // RMSetSriovMode - required when vGPU is enabled.
            entries.push(
                RegistryEntry {
                    key: "RMSetSriovMode",
                    value: 1,
                },
                GFP_KERNEL,
            )?;
        }

        Ok(Self { entries })
    }
}

impl CommandToGsp for SetRegistry {
    const FUNCTION: MsgFunction = MsgFunction::SetRegistry;
    type Command = fw::commands::PackedRegistryTable;
    type Reply = NoReply;
    type InitError = Infallible;

    fn init(&self) -> impl Init<Self::Command, Self::InitError> {
        Self::Command::init(self.entries.len() as u32, self.size() as u32)
    }

    fn variable_payload_len(&self) -> usize {
        let mut key_size = 0;
        for entry in self.entries.iter() {
            key_size += entry.key.len() + 1; // +1 for NULL terminator
        }
        self.entries.len() * size_of::<fw::commands::PackedRegistryEntry>() + key_size
    }

    fn init_variable_payload(
        &self,
        dst: &mut SBufferIter<core::array::IntoIter<&mut [u8], 2>>,
    ) -> Result {
        let string_data_start_offset = size_of::<Self::Command>()
            + self.entries.len() * size_of::<fw::commands::PackedRegistryEntry>();

        // Array for string data.
        let mut string_data = KVec::new();

        for entry in self.entries.iter() {
            dst.write_all(
                fw::commands::PackedRegistryEntry::new(
                    (string_data_start_offset + string_data.len()) as u32,
                    entry.value,
                )
                .as_bytes(),
            )?;

            let key_bytes = entry.key.as_bytes();
            string_data.extend_from_slice(key_bytes, GFP_KERNEL)?;
            string_data.push(0, GFP_KERNEL)?;
        }

        dst.write_all(string_data.as_slice())
    }
}

/// Message type for GSP initialization done notification.
struct GspInitDone;

// SAFETY: `GspInitDone` is a zero-sized type with no bytes, therefore it
// trivially has no uninitialized bytes.
unsafe impl FromBytes for GspInitDone {}

impl MessageFromGsp for GspInitDone {
    const FUNCTION: MsgFunction = MsgFunction::GspInitDone;
    type InitError = Infallible;
    type Message = ();

    fn read(
        _msg: &Self::Message,
        _sbuffer: &mut SBufferIter<array::IntoIter<&[u8], 2>>,
    ) -> Result<Self, Self::InitError> {
        Ok(GspInitDone)
    }
}

/// Waits for GSP initialization to complete.
pub(crate) fn wait_gsp_init_done(cmdq: &Cmdq<'_>) -> Result {
    cmdq.await_msg::<GspInitDone>().map(|_| ())
}

/// The `GetGspStaticInfo` command.
pub(crate) struct GetGspStaticInfo;

impl CommandToGsp for GetGspStaticInfo {
    const FUNCTION: MsgFunction = MsgFunction::GetGspStaticInfo;
    type Command = fw::commands::GspStaticConfigInfo;
    type Reply = GspStaticInfo;
    type InitError = Infallible;

    fn init(&self) -> impl Init<Self::Command, Self::InitError> {
        Self::Command::init_zeroed()
    }
}

/// The static GPU configuration, which GSP-RM reports in reply to [`GetGspStaticInfo`].
pub(crate) struct GspStaticInfo {
    gpu_name: [u8; 64],
    /// BAR1 Page Directory Entry base address.
    pub(crate) bar1_pde_base: u64,
    /// Usable FB (VRAM) regions for driver memory allocation.
    pub(crate) usable_fb_regions: KVec<Range<u64>>,
    /// Exclusive end of the FB physical address space.
    pub(crate) total_fb_end: u64,
}

impl MessageFromGsp for GspStaticInfo {
    const FUNCTION: MsgFunction = MsgFunction::GetGspStaticInfo;
    type Message = fw::commands::GspStaticConfigInfo;
    type InitError = Error;

    fn read(
        msg: &Self::Message,
        _sbuffer: &mut SBufferIter<array::IntoIter<&[u8], 2>>,
    ) -> Result<Self, Self::InitError> {
        let mut usable_fb_regions = KVec::new();
        for region in msg.usable_fb_regions() {
            usable_fb_regions.push(region, GFP_KERNEL)?;
        }
        let total_fb_end = msg.total_fb_end().ok_or(EINVAL)?;

        Ok(GspStaticInfo {
            gpu_name: msg.gpu_name_str(),
            bar1_pde_base: msg.bar1_pde_base(),
            usable_fb_regions,
            total_fb_end,
        })
    }
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
#[expect(dead_code)]
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
#[expect(dead_code)]
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

/// The `UnloadingGuestDriver` command, used to shut down the GSP.
///
/// Only used within the `gsp` module.
pub(super) struct UnloadingGuestDriver {
    level: PowerStateLevel,
}

impl UnloadingGuestDriver {
    /// Creates a new `UnloadingGuestDriver` command for the given [`PowerStateLevel`].
    pub(super) fn new(level: PowerStateLevel) -> Self {
        Self { level }
    }
}

impl CommandToGsp for UnloadingGuestDriver {
    const FUNCTION: MsgFunction = MsgFunction::UnloadingGuestDriver;
    type Command = fw::commands::UnloadingGuestDriver;
    type Reply = UnloadingGuestDriverReply;
    type InitError = Infallible;

    fn init(&self) -> impl Init<Self::Command, Self::InitError> {
        fw::commands::UnloadingGuestDriver::new(self.level)
    }
}

/// The reply from the GSP to the [`UnloadingGuestDriver`] command.
pub(super) struct UnloadingGuestDriverReply;

impl MessageFromGsp for UnloadingGuestDriverReply {
    const FUNCTION: MsgFunction = MsgFunction::UnloadingGuestDriver;
    type InitError = Infallible;
    type Message = ();

    fn read(
        _msg: &Self::Message,
        _sbuffer: &mut SBufferIter<array::IntoIter<&[u8], 2>>,
    ) -> Result<Self, Self::InitError> {
        Ok(UnloadingGuestDriverReply)
    }
}
