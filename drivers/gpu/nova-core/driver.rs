// SPDX-License-Identifier: GPL-2.0

use kernel::{
    auxiliary,
    device::Core,
    pci,
    pci::{
        Class,
        ClassMask,
        Vendor, //
    },
    prelude::*,
    sizes::SZ_16M,
    sync::atomic::{
        Atomic,
        Relaxed, //
    },
    types::ForLt,
};

use crate::{
    gpu::Gpu,
    irq::gsp::GspIrq, //
};

/// Counter for generating unique auxiliary device IDs.
static AUXILIARY_ID_COUNTER: Atomic<u32> = Atomic::new(0);

#[pin_data]
pub(crate) struct NovaCore<'bound> {
    /// GSP event interrupt registration.
    ///
    /// Declared first so it is dropped first: `free_irq` runs (waiting out any in-flight handler)
    /// before the GSP is unloaded (`gpu`) or the BAR mapping is released (`bar`).
    #[pin]
    _gsp_irq: GspIrq<'bound>,
    #[pin]
    pub(crate) gpu: Gpu<'bound>,
    bar: pci::Bar<'bound, BAR0_SIZE>,
    #[allow(clippy::type_complexity)]
    _reg: auxiliary::Registration<'bound, ForLt!(())>,
}

pub(crate) struct NovaCoreDriver;

const BAR0_SIZE: usize = SZ_16M;

pub(crate) type Bar0<'a> = &'a pci::Bar<'a, BAR0_SIZE>;

kernel::pci_device_table!(
    PCI_TABLE,
    MODULE_PCI_TABLE,
    <NovaCoreDriver as pci::Driver>::IdInfo,
    [
        // Modern NVIDIA GPUs will show up as either VGA or 3D controllers.
        (
            pci::DeviceId::from_class_and_vendor(
                Class::DISPLAY_VGA,
                ClassMask::ClassSubclass,
                Vendor::NVIDIA
            ),
            ()
        ),
        (
            pci::DeviceId::from_class_and_vendor(
                Class::DISPLAY_3D,
                ClassMask::ClassSubclass,
                Vendor::NVIDIA
            ),
            ()
        ),
    ]
);

impl pci::Driver for NovaCoreDriver {
    type IdInfo = ();
    type Data<'bound> = NovaCore<'bound>;
    const ID_TABLE: pci::IdTable<Self::IdInfo> = &PCI_TABLE;

    fn probe<'bound>(
        pdev: &'bound pci::Device<Core<'_>>,
        _info: &'bound Self::IdInfo,
    ) -> impl PinInit<Self::Data<'bound>, Error> + 'bound {
        pin_init::pin_init_scope(move || {
            dev_dbg!(pdev, "Probe Nova Core GPU driver.\n");

            pdev.enable_device_mem()?;
            pdev.set_master();

            // A PCI device has one interrupt vector allocation, so it is made here for every
            // subtree nova-core services, and each handler takes the vector for its own subtree.
            let vectors = crate::irq::alloc_vectors(pdev, crate::irq::gsp::GSP_SUBTREE)?;
            let gsp_vector = vectors.vector_for(crate::irq::gsp::GSP_SUBTREE)?;
            let irq_type = vectors.irq_type();

            Ok(try_pin_init!(NovaCore {
                bar: pdev.iomap_region_sized::<BAR0_SIZE>(0, c"nova-core/bar0")?,
                // TODO: Use `&bar` self-referential pin-init syntax once available.
                //
                // SAFETY: `bar` is initialized before this expression is evaluated
                // (`try_pin_init!()` initializes fields in the order they appear here), lives at a
                // pinned stable address, and is dropped after `gpu` (struct field drop order).
                gpu <- Gpu::new(pdev, unsafe { &*core::ptr::from_ref(bar) }, vectors),
                // Quiesce the interrupt tree before registering the handler below.
                _: {
                    // SAFETY: as for the `bar` borrow above.
                    let bar = unsafe { &*core::ptr::from_ref(bar) };
                    crate::irq::gsp::quiesce(bar, gpu.chipset(), irq_type);
                },
                // Register the permanent GSP SWGEN0 handler before enabling the interrupt.
                //
                // SAFETY: `bar` is initialized before this expression is evaluated, lives at a
                // pinned stable address, and is dropped after `_gsp_irq` (declared first, so
                // dropped first), so the handler's borrow stays valid for its whole lifetime.
                // `_gsp_irq` is stored in `NovaCore`, whose `Drop` runs `free_irq`, so the
                // registration is never leaked.
                _gsp_irq <- unsafe {
                    GspIrq::new(
                        pdev,
                        gsp_vector,
                        irq_type,
                        &*core::ptr::from_ref(bar),
                        gpu.cmdq(),
                        gpu.chipset(),
                    )
                },
                // Enable the GSP notification now that the handler is registered, then drain any
                // messages the GSP posted during boot before relying on the interrupt.
                _: {
                    // SAFETY: as for the `bar` borrow above.
                    let bar = unsafe { &*core::ptr::from_ref(bar) };
                    crate::irq::gsp::enable(bar, gpu.chipset(), irq_type);
                    gpu.cmdq().drain()?;
                },
                _reg: auxiliary::Registration::new(
                    pdev.as_ref(),
                    c"nova-drm",
                    // TODO[XARR]: Use XArray or perhaps IDA for proper ID allocation/recycling. For
                    // now, use a simple atomic counter that never recycles IDs.
                    AUXILIARY_ID_COUNTER.fetch_add(1, Relaxed),
                    crate::MODULE_NAME,
                    (),
                )?,
            }))
        })
    }
}
