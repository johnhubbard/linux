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
    irq::{
        gsp::GspIrq,
        SubtreeVectors, //
    },
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
    /// Self-referential borrow of `vectors`, so this does not have to be repeated in the
    /// constructor. Will go away with self-referential pin-init.
    vectors_ref: &'bound SubtreeVectors<'bound>,
    /// PCI interrupt vector allocation. Dropped last (struct field drop order).
    #[pin]
    vectors: SubtreeVectors<'bound>,
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

            Ok(try_pin_init!(NovaCore {
                vectors: crate::irq::alloc_vectors(pdev, crate::irq::gsp::GSP_SUBTREE.into())?,
                // SAFETY: `vectors` is initialized above, lives at a pinned stable address, and
                // is dropped after all fields that use `vectors_ref` (struct field drop order).
                vectors_ref: unsafe { &*core::ptr::from_ref(vectors.as_ref().get_ref()) },
                bar: pdev.iomap_region_sized::<BAR0_SIZE>(0, c"nova-core/bar0")?,
                // TODO: Use `&bar` self-referential pin-init syntax once available.
                //
                // SAFETY: `bar` is initialized before this expression is evaluated
                // (`try_pin_init!()` initializes fields in the order they appear here), lives at a
                // pinned stable address, and is dropped after `gpu` (struct field drop order).
                gpu <- Gpu::new(pdev, unsafe { &*core::ptr::from_ref(bar) }, vectors_ref),
                // Quiesce the interrupt tree before registering the handler below.
                _: {
                    // SAFETY: as for the `bar` borrow above.
                    let bar = unsafe { &*core::ptr::from_ref(bar) };
                    crate::irq::gsp::quiesce(bar, gpu.chipset(), vectors_ref.irq_type());
                },
                // Register the permanent GSP SWGEN0 handler, which enables the interrupt.
                //
                // SAFETY: `bar` and `vectors` are initialized and pinned (see above). `_gsp_irq`
                // is declared before `vectors` in the struct, so it is dropped first, ensuring
                // `free_irq` runs before the vectors are freed. The registration is stored in
                // `NovaCore` and never leaked.
                _gsp_irq <- unsafe {
                    GspIrq::new(
                        pdev,
                        vectors_ref,
                        &*core::ptr::from_ref(bar),
                        gpu.cmdq(),
                        gpu.chipset(),
                    )
                },
                // Drain the messages the GSP posted during boot, before relying on the interrupt.
                _: {
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
