// SPDX-License-Identifier: GPL-2.0

//! Nova Core GPU Driver

use kernel::{
    debugfs,
    driver::Registration,
    pci,
    prelude::*,
    InPlaceModule, //
};

#[macro_use]
mod bitfield;

mod driver;
mod falcon;
mod fb;
mod firmware;
mod fsp;
mod gpu;
mod gsp;
mod mctp;
#[macro_use]
mod num;
mod regs;
mod sbuffer;
mod vbios;

pub(crate) const MODULE_NAME: &core::ffi::CStr = <LocalModule as kernel::ModuleMetadata>::NAME;

// TODO: Move this into per-module data once that exists.
static mut DEBUGFS_ROOT: Option<debugfs::Dir> = None;

/// Guard that clears `DEBUGFS_ROOT` when dropped.
struct DebugfsRootGuard;

impl Drop for DebugfsRootGuard {
    fn drop(&mut self) {
        // SAFETY: This guard is dropped after `_driver` (due to field order),
        // so the driver is unregistered and no probe() can be running.
        unsafe { DEBUGFS_ROOT = None };
    }
}

/// Guard that drops any kept GSP-RM log scopes when the module unloads.
struct KeptLogsGuard;

impl Drop for KeptLogsGuard {
    fn drop(&mut self) {
        // Dropped after `_driver` (no probe() can be running) and before
        // `_debugfs_guard` removes the root, while this module's file ops are
        // still mapped, so the kept debugfs entries can be removed safely.
        gsp::drain_kept_logs();
    }
}

#[pin_data]
struct NovaCoreModule {
    // Fields are dropped in declaration order: `_driver` first (unbinds all
    // devices), then `_kept_logs_guard` removes any kept log entries, then
    // `_debugfs_guard` clears `DEBUGFS_ROOT`.
    #[pin]
    _driver: Registration<pci::Adapter<driver::NovaCoreDriver>>,
    _kept_logs_guard: KeptLogsGuard,
    _debugfs_guard: DebugfsRootGuard,
}

impl InPlaceModule for NovaCoreModule {
    fn init(module: &'static kernel::ThisModule) -> impl PinInit<Self, Error> {
        let dir = debugfs::Dir::new(c"nova-core");

        // SAFETY: We are the only driver code running during init, so there
        // cannot be any concurrent access to `DEBUGFS_ROOT`.
        unsafe { DEBUGFS_ROOT = Some(dir) };

        // SAFETY: Module init runs exactly once, and this is reached before
        // driver registration, so this is the only call and it happens before
        // any probe() can run (i.e. before the lock's first use).
        unsafe { gsp::init_kept_logs() };

        try_pin_init!(Self {
            _driver <- Registration::new(MODULE_NAME, module),
            _kept_logs_guard: KeptLogsGuard,
            _debugfs_guard: DebugfsRootGuard,
        })
    }
}

module! {
    type: NovaCoreModule,
    name: "nova-core",
    authors: ["Danilo Krummrich"],
    description: "Nova Core GPU driver",
    license: "GPL v2",
    firmware: [],
    params: {
        keep_gsp_logs: u8 {
            default: 0,
            description: "If non-zero, keep the GSP-RM log debugfs entries and their DMA buffers alive until the module is unloaded, for post-mortem debugging of probe or unload failures. This leaks those buffers. Default 0 (disabled).",
        },
    },
}

kernel::module_firmware!(firmware::ModInfoBuilder);
