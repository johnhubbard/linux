// SPDX-License-Identifier: GPL-2.0

use kernel::device::Device;
use kernel::types::ARef;
use kernel::{
    device, devres::Devres, error::code::*, firmware, fmt, pci, prelude::*, str::BStr, str::CString,
};

use crate::driver::Bar0;
use crate::regs;
use crate::timer::Timer;
use core::fmt;
use core::time::Duration;

const fn to_lowercase_bytes<const N: usize>(s: &str) -> [u8; N] {
    let src = s.as_bytes();
    let mut dst = [0; N];
    let mut i = 0;

    while i < src.len() && i < N {
        dst[i] = (src[i] as char).to_ascii_lowercase() as u8;
        i += 1;
    }

    dst
}

macro_rules! define_chipset {
    ({ $($variant:ident = $value:expr),* $(,)* }) =>
    {
        /// Enum representation of the GPU chipset.
        #[derive(fmt::Debug)]
        pub(crate) enum Chipset {
            $($variant = $value),*,
        }

        impl Chipset {
            pub(crate) const ALL: &'static [Chipset] = &[
                $( Chipset::$variant, )*
            ];

            pub(crate) const NAMES: [&BStr; Self::ALL.len()] = [
                $( BStr::from_bytes(
                        to_lowercase_bytes::<{ stringify!($variant).len() }>(
                            stringify!($variant)
                        ).as_slice()
                ), )*
            ];
        }
    }
}

define_chipset!({
    // Turing
    TU102 = 0x162,
    TU104 = 0x164,
    TU106 = 0x166,
    TU117 = 0x167,
    TU116 = 0x168,
    // Ampere
    GA102 = 0x172,
    GA103 = 0x173,
    GA104 = 0x174,
    GA106 = 0x176,
    GA107 = 0x177,
    // Ada
    AD102 = 0x192,
    AD103 = 0x193,
    AD104 = 0x194,
    AD106 = 0x196,
    AD107 = 0x197,
});

impl Chipset {
    pub(crate) fn arch(&self) -> Architecture {
        match self {
            Self::TU102 | Self::TU104 | Self::TU106 | Self::TU117 | Self::TU116 => {
                Architecture::Turing
            }
            Self::GA102 | Self::GA103 | Self::GA104 | Self::GA106 | Self::GA107 => {
                Architecture::Ampere
            }
            Self::AD102 | Self::AD103 | Self::AD104 | Self::AD106 | Self::AD107 => {
                Architecture::Ada
            }
        }
    }
}

// TODO
//
// The resulting strings are used to generate firmware paths, hence the
// generated strings have to be stable.
//
// Hence, replace with something like strum_macros derive(Display).
//
// For now, redirect to fmt::Debug for convenience.
impl fmt::Display for Chipset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

// TODO replace with something like derive(FromPrimitive)
impl TryFrom<u32> for Chipset {
    type Error = kernel::error::Error;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0x162 => Ok(Chipset::TU102),
            0x164 => Ok(Chipset::TU104),
            0x166 => Ok(Chipset::TU106),
            0x167 => Ok(Chipset::TU117),
            0x168 => Ok(Chipset::TU116),
            0x172 => Ok(Chipset::GA102),
            0x173 => Ok(Chipset::GA103),
            0x174 => Ok(Chipset::GA104),
            0x176 => Ok(Chipset::GA106),
            0x177 => Ok(Chipset::GA107),
            0x192 => Ok(Chipset::AD102),
            0x193 => Ok(Chipset::AD103),
            0x194 => Ok(Chipset::AD104),
            0x196 => Ok(Chipset::AD106),
            0x197 => Ok(Chipset::AD107),
            _ => Err(ENODEV),
        }
    }
}

/// Enum representation of the GPU generation.
#[derive(fmt::Debug)]
pub(crate) enum Architecture {
    Turing,
    Ampere,
    Ada,
}

pub(crate) struct Revision {
    major: u8,
    minor: u8,
}

impl Revision {
    fn from_boot0(boot0: regs::Boot0) -> Self {
        Self {
            major: boot0.major_rev(),
            minor: boot0.minor_rev(),
        }
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}.{:x}", self.major, self.minor)
    }
}

/// Structure holding the metadata of the GPU.
pub(crate) struct Spec {
    chipset: Chipset,
    /// The revision of the chipset.
    revision: Revision,
}

impl Spec {
    fn new(bar: &Devres<Bar0>) -> Result<Spec> {
        let bar = bar.try_access().ok_or(ENXIO)?;
        let boot0 = regs::Boot0::read(&bar);

        Ok(Self {
            chipset: boot0.chipset().try_into()?,
            revision: Revision::from_boot0(boot0),
        })
    }
}

/// Structure encapsulating the firmware blobs required for the GPU to operate.
#[expect(dead_code)]
pub(crate) struct Firmware {
    booter_load: firmware::Firmware,
    booter_unload: firmware::Firmware,
    bootloader: firmware::Firmware,
    gsp: firmware::Firmware,
}

impl Firmware {
    fn new(dev: &device::Device, spec: &Spec, ver: &str) -> Result<Firmware> {
        let mut chip_name = CString::try_from_fmt(fmt!("{}", spec.chipset))?;
        chip_name.make_ascii_lowercase();

        let request = |name_| {
            CString::try_from_fmt(fmt!("nvidia/{}/gsp/{}-{}.bin", &*chip_name, name_, ver))
                .and_then(|path| firmware::Firmware::request(&path, dev))
        };

        Ok(Firmware {
            booter_load: request("booter_load")?,
            booter_unload: request("booter_unload")?,
            bootloader: request("bootloader")?,
            gsp: request("gsp")?,
        })
    }
}

/// Structure holding the resources required to operate the GPU.
#[pin_data]
pub(crate) struct Gpu {
    dev: ARef<Device>,
    spec: Spec,
    /// MMIO mapping of PCI BAR 0
    bar: Devres<Bar0>,
    fw: Firmware,
    timer: Timer,
}

impl Gpu {
    pub(crate) fn new(pdev: &pci::Device, bar: Devres<Bar0>) -> Result<impl PinInit<Self>> {
        let spec = Spec::new(&bar)?;
        let fw = Firmware::new(pdev.as_ref(), &spec, "535.113.01")?;

        dev_info!(
            pdev.as_ref(),
            "NVIDIA (Chipset: {}, Architecture: {:?}, Revision: {})\n",
            spec.chipset,
            spec.chipset.arch(),
            spec.revision
        );

        let dev = pdev.as_ref().into();
        let timer = Timer::new();

        Ok(pin_init!(Self {
            dev,
            spec,
            bar,
            fw,
            timer,
        }))
    }

    pub(crate) fn test_timer(&self) -> Result<()> {
        let bar = self.bar.try_access().ok_or(ENXIO)?;

        dev_info!(&self.dev, "testing timer subdev\n");
        assert!(matches!(
            self.timer
                .wait_on(&bar, Duration::from_millis(10), || Some(())),
            Ok(())
        ));
        assert_eq!(
            self.timer
                .wait_on(&bar, Duration::from_millis(10), || Option::<()>::None),
            Err(ETIMEDOUT)
        );

        Ok(())
    }
}
