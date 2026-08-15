// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Loading of the ucodes image, the bindata blob of microcode that GSP-RM loads at run time.

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

/// Loads the ucodes image that the `ucodes` metadata file names.
///
/// # Errors
///
/// - `ENOENT` if the metadata file or the image it names is not installed.
/// - `EINVAL` if the metadata is malformed or lacks the `FILE` or `SIZE` tag.
/// - `ENODATA` if `SIZE` is zero.
/// - `ENOMEM` if the image buffer cannot be allocated.
#[expect(dead_code)]
pub(crate) fn request_ucodes_firmware(dev: &device::Device, chipset: Chipset) -> Result<VVec<u8>> {
    let firmware = request_tlv(dev, chipset, "ucodes")?;
    let tlv = Tlv::new(firmware.data())?;

    tlv.load_file(dev, chipset)
}
