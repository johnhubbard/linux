// SPDX-License-Identifier: GPL-2.0

use kernel::{
    device,
    dma::{
        Coherent,
        DmaAddress, //
    },
    firmware,
    prelude::*, //
};

use crate::{
    firmware::{
        radix3::Radix3,
        riscv::RiscvFirmware, //
        tlv::{
            request_tlv, //
            Tlv,
        },
    },
    gpu::Chipset, //
};

/// Longest build ID that a log dump header carries, matching Open RM's `BUILD_ID_MAX_LENGTH`.
pub(crate) const BUILD_ID_MAX_LENGTH: usize = 32;

/// Build ID of the GSP firmware, from the `BLID` tag of its TLV.
pub(crate) struct BuildId {
    /// The ID, zero-padded to [`BUILD_ID_MAX_LENGTH`] bytes.
    bytes: [u8; BUILD_ID_MAX_LENGTH],
    /// Number of valid bytes in `bytes`.
    len: u32,
}

impl BuildId {
    /// Reads the build ID from the `BLID` tag of `tlv`.
    ///
    /// Returns `EINVAL` if the tag is absent or empty, or if its value is longer than
    /// [`BUILD_ID_MAX_LENGTH`] bytes.
    pub(crate) fn from_tlv(tlv: &Tlv<'_>) -> Result<Self> {
        let value = tlv.get_bytes(b"BLID")?;

        let mut bytes = [0; BUILD_ID_MAX_LENGTH];
        bytes
            .get_mut(..value.len())
            .ok_or(EINVAL)?
            .copy_from_slice(value);

        Ok(Self {
            bytes,
            len: u32::try_from(value.len())?,
        })
    }

    /// Returns the ID, zero-padded to [`BUILD_ID_MAX_LENGTH`] bytes.
    pub(crate) fn padded(&self) -> &[u8; BUILD_ID_MAX_LENGTH] {
        &self.bytes
    }

    /// Returns the number of bytes in the ID.
    pub(crate) fn len(&self) -> u32 {
        self.len
    }
}

/// The GSP firmware image, its signatures, and the GSP bootloader.
#[pin_data]
pub(crate) struct GspFirmware<'a> {
    /// The firmware image and the radix3 table that maps it.
    #[pin]
    radix3: Radix3<'a>,
    /// Device-mapped GSP signatures matching the GPU's [`Chipset`].
    pub(crate) signatures: Coherent<'a, [u8]>,
    /// GSP bootloader, verifies the GSP firmware before loading and running it.
    pub(crate) bootloader: RiscvFirmware<'a>,
}

impl<'a> GspFirmware<'a> {
    /// Loads the GSP firmware binaries, map them into `dev`'s address-space, and creates the page
    /// tables expected by the GSP bootloader to load it.
    ///
    /// `gsp_tlv` is the TLV of the GSP firmware, which names the image file and carries the
    /// signatures.
    pub(crate) fn new<'tlv>(
        dev: &'a device::Device<device::Bound>,
        chipset: Chipset,
        gsp_tlv: &'tlv firmware::Firmware,
    ) -> impl PinInit<Self, Error> + use<'a, 'tlv> {
        pin_init::pin_init_scope(move || {
            let tlv = Tlv::new(gsp_tlv.data())?;
            dev_dbg!(dev, "loaded gsp firmware v{}\n", tlv.get_string(b"VERS")?);

            let fw_vvec = tlv.load_file(dev, chipset)?;

            let signatures = Coherent::from_slice(dev, tlv.get_bytes(b"SIGN")?, GFP_KERNEL)?;

            Ok(try_pin_init!(Self {
                radix3 <- Radix3::new(dev, fw_vvec),
                signatures,
                bootloader: {
                    let bl = request_tlv(dev, chipset, "gsp_bootloader")?;

                    RiscvFirmware::new(dev, &bl)?
                },
            }))
        })
    }

    /// Returns the size of the firmware image, in bytes.
    pub(crate) fn size(&self) -> usize {
        self.radix3.size()
    }

    /// Returns the DMA address of the radix3 table that maps the firmware image.
    pub(crate) fn radix3_dma_address(&self) -> DmaAddress {
        self.radix3.dma_address()
    }
}
