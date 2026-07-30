// SPDX-License-Identifier: GPL-2.0

//! PCI interrupt infrastructure.

use super::Device;
use crate::{
    bindings,
    device,
    device::Bound,
    devres,
    error::to_result,
    irq::{
        self,
        IrqRequest, //
    },
    prelude::*,
    str::CStr,
    sync::aref::ARef, //
};
use core::num::NonZero;

/// IRQ type flags for PCI interrupt allocation.
#[derive(Debug, Clone, Copy)]
pub enum IrqType {
    /// INTx interrupts.
    Intx,
    /// Message Signaled Interrupts (MSI).
    Msi,
    /// Extended Message Signaled Interrupts (MSI-X).
    MsiX,
}

impl IrqType {
    /// Convert to the corresponding kernel flags.
    const fn as_raw(self) -> u32 {
        match self {
            IrqType::Intx => bindings::PCI_IRQ_INTX,
            IrqType::Msi => bindings::PCI_IRQ_MSI,
            IrqType::MsiX => bindings::PCI_IRQ_MSIX,
        }
    }
}

/// Set of IRQ types that can be used for PCI interrupt allocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct IrqTypes(u32);

impl IrqTypes {
    /// Create a set containing all IRQ types (MSI-X, MSI, and INTx).
    pub const fn all() -> Self {
        Self(bindings::PCI_IRQ_ALL_TYPES)
    }

    /// Build a set of IRQ types.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Create a set with only MSI and MSI-X (no INTx interrupts).
    /// let msi_only = IrqTypes::default()
    ///     .with(IrqType::Msi)
    ///     .with(IrqType::MsiX);
    /// ```
    pub const fn with(self, irq_type: IrqType) -> Self {
        Self(self.0 | irq_type.as_raw())
    }

    /// Get the raw flags value.
    const fn as_raw(self) -> u32 {
        self.0
    }
}

/// A Linux IRQ number belonging to one PCI device's interrupt allocation.
///
/// [`IrqAllocation::vector`] resolves a vector index to one of these, and
/// [`Device::request_irq`] or [`Device::request_threaded_irq`] registers a handler on it.
///
/// # Invariants
///
/// `irq` is a Linux IRQ number of `dev`.
#[derive(Clone, Copy)]
pub struct IrqVector<'a> {
    dev: &'a Device<Bound>,
    irq: u32,
}

impl<'a> From<IrqVector<'a>> for IrqRequest<'a> {
    fn from(vector: IrqVector<'a>) -> Self {
        // SAFETY: By the type invariant, `irq` is a Linux IRQ number of `dev`.
        unsafe { IrqRequest::new(vector.dev.as_ref(), vector.irq) }
    }
}

/// An allocation of PCI interrupt vectors for a device.
///
/// [`Device::alloc_irq_vectors`] allocates the vectors and returns this handle. The vectors are
/// numbered `0..count`, and [`Self::vector`] resolves one of those indices to the Linux IRQ
/// number that delivers it.
///
/// # Invariants
///
/// `dev` has an allocation of `count` interrupt vectors of type `irq_type`.
#[derive(Clone, Copy)]
pub struct IrqAllocation<'a> {
    dev: &'a Device<Bound>,
    count: NonZero<u32>,
    irq_type: IrqType,
}

impl<'a> IrqAllocation<'a> {
    /// Returns the number of vectors that were allocated.
    ///
    /// This is at least the `min_vecs` that [`Device::alloc_irq_vectors`] was asked for.
    pub fn count(&self) -> NonZero<u32> {
        self.count
    }

    /// Returns the interrupt type the PCI core selected.
    ///
    /// [`Device::alloc_irq_vectors`] takes a set of acceptable types and picks one of them, so a
    /// driver whose behavior depends on the type asks for it here rather than assuming. Every
    /// vector of the allocation has this type.
    pub fn irq_type(&self) -> IrqType {
        self.irq_type
    }

    /// Resolves the vector at `index` to the Linux IRQ number that delivers it.
    ///
    /// # Errors
    ///
    /// - `EINVAL` if `index` is outside the allocation.
    /// - The error `pci_irq_vector()` returns if the PCI core has no IRQ number for `index`.
    pub fn vector(&self, index: u32) -> Result<IrqVector<'a>> {
        if index >= self.count.get() {
            return Err(EINVAL);
        }

        // SAFETY: `self.dev.as_raw()` is a valid pointer to a `struct pci_dev`.
        let irq = unsafe { bindings::pci_irq_vector(self.dev.as_raw(), index) };
        if irq < 0 {
            return Err(crate::error::Error::from_errno(irq));
        }

        // INVARIANT: `pci_irq_vector` returned a Linux IRQ number of `dev`.
        Ok(IrqVector {
            dev: self.dev,
            irq: irq as u32,
        })
    }
}

/// Represents an IRQ vector allocation for a PCI device.
///
/// This type ensures that IRQ vectors are properly allocated and freed by
/// tying the allocation to the lifetime of this registration object.
///
/// # Invariants
///
/// The [`Device`] has successfully allocated IRQ vectors.
struct IrqVectorRegistration {
    dev: ARef<Device>,
}

impl IrqVectorRegistration {
    /// Allocate and register IRQ vectors for the given PCI device.
    ///
    /// Allocates IRQ vectors and registers them with devres for automatic cleanup.
    /// Returns a handle to the allocated IRQ vectors.
    fn register<'a>(
        dev: &'a Device<Bound>,
        min_vecs: u32,
        max_vecs: u32,
        irq_types: IrqTypes,
    ) -> Result<IrqAllocation<'a>> {
        // SAFETY:
        // - `dev.as_raw()` is guaranteed to be a valid pointer to a `struct pci_dev`
        //   by the type invariant of `Device`.
        // - `pci_alloc_irq_vectors` internally validates all other parameters
        //   and returns error codes.
        let ret = unsafe {
            bindings::pci_alloc_irq_vectors(dev.as_raw(), min_vecs, max_vecs, irq_types.as_raw())
        };

        to_result(ret)?;

        // `pci_alloc_irq_vectors` returns the number of vectors it allocated.
        let count = NonZero::new(ret as u32).ok_or(EINVAL)?;

        // SAFETY: `dev.as_raw()` is a valid pointer to a `struct pci_dev`.
        let irq_type = match unsafe { bindings::pci_irq_type(dev.as_raw()) } {
            bindings::PCI_IRQ_MSIX => IrqType::MsiX,
            bindings::PCI_IRQ_MSI => IrqType::Msi,
            // The helper returns `PCI_IRQ_INTX` when neither MSI nor MSI-X is enabled.
            _ => IrqType::Intx,
        };

        // INVARIANT: `pci_alloc_irq_vectors` allocated `count` vectors of `irq_type` for `dev`,
        // numbered from 0.
        let vectors = IrqAllocation {
            dev,
            count,
            irq_type,
        };

        // INVARIANT: The IRQ vector allocation for `dev` above was successful.
        let irq_vecs = Self { dev: dev.into() };
        devres::register(dev.as_ref(), irq_vecs, GFP_KERNEL)?;

        Ok(vectors)
    }
}

impl Drop for IrqVectorRegistration {
    fn drop(&mut self) {
        // SAFETY:
        // - By the type invariant, `self.dev.as_raw()` is a valid pointer to a `struct pci_dev`.
        // - `self.dev` has successfully allocated IRQ vectors.
        unsafe { bindings::pci_free_irq_vectors(self.dev.as_raw()) };
    }
}

impl Device<device::Bound> {
    /// Returns a [`kernel::irq::Registration`] for the given IRQ vector.
    ///
    /// # Safety
    ///
    /// Callers must not `mem::forget()` the resulting [`irq::Registration`] or otherwise prevent
    /// its [`Drop`] implementation from running.
    pub unsafe fn request_irq<'a, T: crate::irq::Handler + 'a>(
        &'a self,
        vector: IrqVector<'a>,
        flags: irq::Flags,
        name: &'static CStr,
        handler: impl PinInit<T, Error> + 'a,
    ) -> impl PinInit<irq::Registration<'a, T>, Error> + 'a {
        // SAFETY: Caller guarantees the Registration will not be leaked.
        unsafe { irq::Registration::<T>::new(vector.into(), flags, name, handler) }
    }

    /// Returns a [`kernel::irq::ThreadedRegistration`] for the given IRQ vector.
    ///
    /// # Safety
    ///
    /// Callers must not `mem::forget()` the resulting [`irq::ThreadedRegistration`] or otherwise
    /// prevent its [`Drop`] implementation from running.
    pub unsafe fn request_threaded_irq<'a, T: crate::irq::ThreadedHandler + 'a>(
        &'a self,
        vector: IrqVector<'a>,
        flags: irq::Flags,
        name: &'static CStr,
        handler: impl PinInit<T, Error> + 'a,
    ) -> impl PinInit<irq::ThreadedRegistration<'a, T>, Error> + 'a {
        // SAFETY: Caller guarantees the Registration will not be leaked.
        unsafe { irq::ThreadedRegistration::<T>::new(vector.into(), flags, name, handler) }
    }

    /// Allocate IRQ vectors for this PCI device with automatic cleanup.
    ///
    /// Allocates between `min_vecs` and `max_vecs` interrupt vectors for the device.
    /// The allocation will use MSI-X, MSI, or INTx interrupts based on the `irq_types`
    /// parameter and hardware capabilities. When multiple types are specified, the kernel
    /// will try them in order of preference: MSI-X first, then MSI, then INTx interrupts.
    ///
    /// The allocated vectors are automatically freed when the device is unbound, using the
    /// devres (device resource management) system.
    ///
    /// # Arguments
    ///
    /// * `min_vecs` - Minimum number of vectors required.
    /// * `max_vecs` - Maximum number of vectors to allocate.
    /// * `irq_types` - Types of interrupts that can be used.
    ///
    /// # Returns
    ///
    /// Returns the IRQ vector allocation, or an error if `min_vecs` vectors cannot be allocated.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{ device::Bound, pci};
    /// # fn no_run(dev: &pci::Device<Bound>) -> Result {
    /// // Allocate using any available interrupt type in the order mentioned above.
    /// let vectors = dev.alloc_irq_vectors(1, 32, pci::IrqTypes::all())?;
    ///
    /// // Allocate MSI or MSI-X only (no INTx interrupts).
    /// let msi_only = pci::IrqTypes::default()
    ///     .with(pci::IrqType::Msi)
    ///     .with(pci::IrqType::MsiX);
    /// let vectors = dev.alloc_irq_vectors(4, 16, msi_only)?;
    ///
    /// // Resolve every allocated vector to the IRQ number a handler is registered on.
    /// for index in 0..vectors.count().get() {
    ///     let _vector = vectors.vector(index)?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn alloc_irq_vectors(
        &self,
        min_vecs: u32,
        max_vecs: u32,
        irq_types: IrqTypes,
    ) -> Result<IrqAllocation<'_>> {
        IrqVectorRegistration::register(self, min_vecs, max_vecs, irq_types)
    }
}
