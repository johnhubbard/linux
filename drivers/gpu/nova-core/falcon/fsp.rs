// SPDX-License-Identifier: GPL-2.0

// TODO: remove this once the code is fully functional
#![allow(dead_code)]

use crate::driver::Bar0;
use crate::falcon::{Falcon, FalconEngine};
use crate::regs;
use kernel::prelude::*;

/// Type specifying the `Fsp` falcon engine. Cannot be instantiated.
pub(crate) struct Fsp(());

impl FalconEngine for Fsp {
    // FSP falcon base address:
    const BASE: usize = 0x8f2000;
}

impl Falcon<Fsp> {
    /// Write data to FSP external memory using Falcon PIO (Programmed I/O).
    ///
    /// This function writes data to the FSP (Falcon Security Processor) external
    /// memory space using the Falcon's indirect memory access interface.
    ///
    /// # Arguments
    /// * `dev` - Device for logging FSP emem operations
    /// * `bar` - BAR0 memory mapping for register access
    /// * `offset` - Byte offset within FSP external memory to start writing
    /// * `data` - Slice of bytes to write to memory (must be 4-byte aligned)
    ///
    /// # Returns
    /// `Ok(())` on successful write, or an error if register operations fail.
    ///
    /// # Note
    /// The data length must be 4-byte aligned as required by the falcon hardware.
    pub(crate) fn write_emem(&self, bar: &Bar0, offset: u32, data: &[u8]) -> Result {
        // Use GP102 EMEM PIO registers for FSP
        // Initialize EMEM write: BIT(24) | emem_base
        regs::NV_PFALCON_FALCON_EMEM_CTL::default()
            .set_value((1 << 24) | offset)
            .write(bar, Fsp::BASE);

        // Write data in 4-byte chunks using GP102 EMEM data register
        // Write data in 4-byte chunks using GP102 EMEM data register
        for chunk in data.chunks(4) {
            let mut word = 0u32;
            for (i, &byte) in chunk.iter().enumerate() {
                word |= (byte as u32) << (i * 8);
            }

            regs::NV_PFALCON_FALCON_EMEM_DATA::default()
                .set_data(word)
                .write(bar, Fsp::BASE);
        }

        Ok(())
    }

    /// Read data from FSP external memory using Falcon PIO (Programmed I/O).
    ///
    /// This function reads data from the FSP (Falcon Security Processor) external
    /// memory space using the Falcon's indirect memory access interface.
    ///
    /// # Arguments
    /// * `dev` - Device for logging FSP emem operations
    /// * `bar` - BAR0 memory mapping for register access
    /// * `offset` - Byte offset within FSP external memory to start reading
    /// * `data` - Mutable slice to store the read data (must be 4-byte aligned)
    ///
    /// # Returns
    /// `Ok(())` on successful read, or an error if register operations fail.
    ///
    /// # Note
    /// The data length must be 4-byte aligned as required by the falcon hardware.
    pub(crate) fn read_emem(&self, bar: &Bar0, offset: u32, data: &mut [u8]) -> Result {
        // Use GP102 EMEM PIO registers for FSP
        // Initialize EMEM read: BIT(25) | emem_base (different from write which uses BIT(24))
        regs::NV_PFALCON_FALCON_EMEM_CTL::default()
            .set_value((1 << 25) | offset)
            .write(bar, Fsp::BASE);

        // Read data in 4-byte chunks using GP102 EMEM data register
        for chunk in data.chunks_mut(4) {
            let word = regs::NV_PFALCON_FALCON_EMEM_DATA::read(bar, Fsp::BASE).data();

            for (i, byte) in chunk.iter_mut().enumerate() {
                *byte = ((word >> (i * 8)) & 0xff) as u8;
            }
        }

        Ok(())
    }
}
