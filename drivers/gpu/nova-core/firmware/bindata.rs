// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! GSP bindata firmware loading.

use kernel::{
    device,
    prelude::*, //
};

use crate::{
    firmware::tlv::{
        request_tlv,
        Tlv, //
    },
    gpu::Chipset,
};

/// Requests the optional `ucodes` bindata metadata and loads its payload.
///
/// A missing metadata file is reported as [`None`] so that the eventual caller can decide whether
/// ucodes are optional for its boot path. Once the metadata has been found, all parse and payload
/// loading errors, including a missing referenced file, are returned as errors.
pub(crate) fn request_ucodes_firmware(
    dev: &device::Device,
    chipset: Chipset,
) -> Result<Option<VVec<u8>>> {
    let firmware = match request_tlv(dev, chipset, "ucodes") {
        Ok(firmware) => firmware,
        Err(e) if e == ENOENT => return Ok(None),
        Err(e) => return Err(e),
    };

    let tlv = Tlv::new(firmware.data())?;

    Ok(Some(tlv.load_blob_or_file(dev, chipset)?))
}
