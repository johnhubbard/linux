// SPDX-License-Identifier: GPL-2.0

use kernel::io::register;

use crate::regs::NV_PBUS_SW_SCRATCH;

// PGSP
//
// The msgq v2 wire protocol replaces the in-memory ring pointers with four
// BAR0 registers per queue:
//
//     NV_PGSP_QUEUE_HEAD[i] @ 0x00110c00 + i*8  (CPU TX write, doorbell)
//     NV_PGSP_QUEUE_TAIL[i] @ 0x00110c04 + i*8  (GSP TX read)
//     NV_PGSP_MSGQ_HEAD[i]  @ 0x00110c80 + i*8  (GSP RX write)
//     NV_PGSP_MSGQ_TAIL[i]  @ 0x00110c84 + i*8  (CPU RX read)
//
// Nova only uses queue 0, so the four registers are declared as single
// scalars at the i=0 offsets. NV_PGSP_QUEUE_HEAD is also used as the v0
// doorbell, which is why it predates the others.

register! {
    pub(super) NV_PGSP_QUEUE_HEAD(u32) @ 0x00110c00 {
        31:0    address;
    }
}

register! {
    pub(super) NV_PGSP_QUEUE_TAIL(u32) @ 0x00110c04 {
        31:0    address;
    }
}

register! {
    pub(super) NV_PGSP_MSGQ_HEAD(u32) @ 0x00110c80 {
        31:0    address;
    }
}

register! {
    pub(super) NV_PGSP_MSGQ_TAIL(u32) @ 0x00110c84 {
        31:0    address;
    }
}

// PBUS

register! {
    /// Scratch register 0xe used as FRTS firmware error code.
    pub(super) NV_PBUS_SW_SCRATCH_0E_FRTS_ERR(u32) => NV_PBUS_SW_SCRATCH[0xe] {
        31:16   frts_err_code;
    }
}
