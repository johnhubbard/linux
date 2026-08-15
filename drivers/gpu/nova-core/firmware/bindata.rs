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

/// Loads the payload that the `ucodes` bindata metadata names.
///
/// An absent metadata file yields `None` rather than an error, because whether ucodes are
/// required depends on the boot path.
///
/// # Errors
///
/// - `ENOENT` if the metadata names a file that is not installed.
/// - Errors from parsing the metadata and from [`Tlv::load_file`] are propagated as-is.
#[expect(dead_code)]
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
    let (_, ucodes) = tlv.load_file(dev, chipset)?;

    Ok(Some(ucodes))
}
