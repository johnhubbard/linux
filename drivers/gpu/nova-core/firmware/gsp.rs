// SPDX-License-Identifier: GPL-2.0

use kernel::{
    device,
    dma::{
        Coherent,
        DmaAddress, //
    },
    prelude::*,
    str::CString,
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

/// GSP firmware with 3-level radix page tables for the GSP bootloader.
///
/// Also known as "Radix3" firmware.
#[pin_data]
pub(crate) struct GspFirmware {
    /// The GSP firmware image mapped via a 3-level radix page table.
    #[pin]
    radix3: Radix3,
    /// Firmware file path requested from userspace.
    pub(crate) fw_path: CString,
    /// Firmware version from the TLV metadata.
    pub(crate) fw_version: CString,
    /// Device-mapped GSP signatures matching the GPU's [`Chipset`].
    pub(crate) signatures: Coherent<[u8]>,
    /// GSP bootloader, verifies the GSP firmware before loading and running it.
    pub(crate) bootloader: RiscvFirmware,
}

impl GspFirmware {
    /// Loads the GSP firmware binaries, map them into `dev`'s address-space, and creates the page
    /// tables expected by the GSP bootloader to load it.
    pub(crate) fn new<'a>(
        dev: &'a device::Device<device::Bound>,
        chipset: Chipset,
    ) -> impl PinInit<Self, Error> + 'a {
        pin_init::pin_init_scope(move || {
            let firmware = request_tlv(dev, chipset, "gsp")?;
            let tlv = Tlv::new(firmware.data())?;
            let fw_version = CString::try_from_fmt(fmt!("{}", tlv.get_string(b"VERS")?))?;
            dev_dbg!(
                dev,
                "loaded gsp firmware v{}\n",
                fw_version.to_str().unwrap_or("unknown")
            );

            let (fw_path, fw_vvec) = tlv.load_file(dev, chipset)?;

            let signatures = Coherent::from_slice(dev, tlv.get_bytes(b"SIGN")?, GFP_KERNEL)?;

            Ok(try_pin_init!(Self {
                radix3 <- Radix3::new(dev, fw_vvec),
                fw_path,
                fw_version,
                signatures,
                bootloader: {
                    let bl = request_tlv(dev, chipset, "gsp_bootloader")?;

                    RiscvFirmware::new(dev, &bl)?
                },
            }))
        })
    }

    /// Returns the size of the GSP firmware image, in bytes.
    pub(crate) fn size(&self) -> usize {
        self.radix3.size()
    }

    /// Returns the DMA address of the radix3 level 0 page table.
    pub(crate) fn radix3_dma_address(&self) -> DmaAddress {
        self.radix3.dma_address()
    }
}
