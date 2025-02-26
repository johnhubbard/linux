// SPDX-License-Identifier: GPL-2.0

use crate::gpu;
use kernel::firmware;

pub(crate) struct ModInfoBuilder<const N: usize>(firmware::ModInfoBuilder<N>);

impl<const N: usize> ModInfoBuilder<N> {
    const fn make_entry_file(self, chipset: &[u8], fw: &[u8]) -> Self {
        let version = b"535.113.01";

        ModInfoBuilder(
            self.0
                .prepare()
                .push(b"nvidia/")
                .push(chipset)
                .push(b"/gsp/")
                .push(fw)
                .push(b"-")
                .push(version)
                .push(b".bin"),
        )
    }

    const fn make_entry_chipset(self, chipset: &[u8]) -> Self {
        self.make_entry_file(chipset, b"booter_load")
            .make_entry_file(chipset, b"booter_unload")
            .make_entry_file(chipset, b"bootloader")
            .make_entry_file(chipset, b"gsp")
    }

    pub(crate) const fn create(
        module_name: &'static kernel::str::CStr,
    ) -> firmware::ModInfoBuilder<N> {
        let mut this = Self(firmware::ModInfoBuilder::new(module_name));
        let mut i = 0;

        while i < gpu::Chipset::NAMES.len() {
            this = this.make_entry_chipset(gpu::Chipset::NAMES[i].as_bytes());
            i += 1;
        }

        this.0
    }
}
