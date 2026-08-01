// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! The GIN CPU interrupt tree for one PCIe function.
//!
//! A vector's number fixes where it latches: leaf `vector / 32` at bit `vector % 32`, and that
//! leaf belongs to subtree `vector / 64`. The types here keep those three views apart, so a leaf
//! index, a set of vectors within one leaf, and a `TOP` bit cannot stand in for one another.
//!
//! Servicing a leaf has a required order: read its pending bits, then clear them. Only
//! [`Tree::read_pending`] produces a [`LeafPending`], and only a [`LeafPending`] clears, so the
//! wrong order does not compile. Serializing access to the tree is the caller's responsibility.
//!
//! See `Documentation/gpu/nova/core/interrupts.rst`.

use kernel::{
    io::{
        register::Array,
        Io, //
    },
    num::Bounded,
    prelude::*, //
};

use crate::{
    driver::Bar0,
    gpu::Chipset,
    num, //
};

use super::{
    hal::{
        cpu_interrupt_hal,
        PciIrqRearmMethod, //
    },
    regs::*,
    MsiType, //
};

/// Number of bits a leaf index occupies, covering the 16 entries of the leaf register arrays.
const LEAF_INDEX_BITS: u32 = 4;

/// Index of a leaf register.
pub(super) type LeafIndex = Bounded<usize, LEAF_INDEX_BITS>;

/// Number of vectors one leaf register carries, one per bit.
const VECTORS_PER_LEAF: u32 = 32;

/// Number of leaves one subtree covers.
const LEAVES_PER_SUBTREE: u32 = 2;

/// Number of bits that address any vector the widest supported tree carries.
const VECTOR_BITS: u32 = 9;

const _: () = assert!(1 << VECTOR_BITS == LeafCount::Sixteen.vector_count());

/// Width of the vector field in the leaf trigger register.
const TRIGGER_VECTOR_BITS: u32 = 12;

/// Number of leaves a tree implements.
///
/// Every supported part implements one of these two counts, and the interrupt HAL names the one
/// its architecture uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub(super) enum LeafCount {
    /// Turing through Ada.
    Eight = 8,

    /// Hopper and later.
    Sixteen = 16,
}

impl LeafCount {
    /// Returns the number of leaves.
    pub(super) const fn into_u32(self) -> u32 {
        // CAST: both discriminants are 16 or below.
        self as u32
    }

    /// Returns the number of leaves, in the type that indexes the leaf register arrays.
    pub(super) const fn into_raw(self) -> usize {
        num::u32_as_usize(self.into_u32())
    }

    /// Returns the number of subtrees, each of which covers two leaves.
    pub(super) const fn subtree_count(self) -> u32 {
        self.into_u32() / LEAVES_PER_SUBTREE
    }

    /// Returns the set of every subtree a tree of this size implements.
    pub(super) const fn subtree_set(self) -> SubtreeSet {
        SubtreeSet((1u32 << self.subtree_count()) - 1)
    }

    /// Returns the number of vectors a tree of this size carries.
    pub(super) const fn vector_count(self) -> u32 {
        self.into_u32() * VECTORS_PER_LEAF
    }
}

/// Set of vectors within one leaf, one bit per vector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LeafMask(u32);

impl LeafMask {
    /// Returns the mask with every vector of the leaf set.
    pub(super) const fn all() -> Self {
        Self(u32::MAX)
    }

    /// Returns the mask holding the vectors set in `raw`.
    pub(super) const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the mask as the value the leaf registers take.
    pub(super) const fn into_raw(self) -> u32 {
        self.0
    }

    /// Returns whether no vector is set.
    pub(super) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns whether every vector set in `other` is also set here.
    pub(super) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One subtree, named by its `TOP` bit.
///
/// # Invariants
///
/// Exactly one bit is set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Subtree(u32);

impl Subtree {
    /// Returns this subtree's index within the tree.
    ///
    /// Under MSI-X this is also the index of the allocated entry the subtree raises.
    pub(super) const fn index(self) -> u32 {
        self.0.trailing_zeros()
    }

    /// Returns the subtree as the value the `TOP` enable registers take.
    pub(super) const fn into_raw(self) -> u32 {
        self.0
    }
}

/// Set of subtrees, one bit per subtree, in the layout the `TOP` enable registers take.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubtreeSet(u32);

impl SubtreeSet {
    /// Returns whether `subtree` belongs to this set.
    pub(super) const fn contains(self, subtree: Subtree) -> bool {
        self.0 & subtree.into_raw() != 0
    }

    /// Returns whether the set holds no subtree.
    pub(super) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns the subtrees present in both sets.
    pub(super) const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Returns the number of subtrees counted from subtree `0` through the highest one in this
    /// set, which is `0` for an empty set.
    pub(super) const fn span(self) -> u32 {
        u32::BITS - self.0.leading_zeros()
    }
}

impl From<Subtree> for SubtreeSet {
    fn from(subtree: Subtree) -> Self {
        Self(subtree.into_raw())
    }
}

/// A GIN interrupt vector, bounded to the widest tree any supported part implements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct GinVector(Bounded<u32, VECTOR_BITS>);

impl GinVector {
    /// Returns the vector numbered `VECTOR`.
    ///
    /// Fails at build time if `VECTOR` lies outside the widest tree any supported part
    /// implements.
    pub(super) const fn new<const VECTOR: u32>() -> Self {
        Self(Bounded::<u32, VECTOR_BITS>::new::<VECTOR>())
    }

    /// Returns the vector number.
    pub(super) const fn into_raw(self) -> u32 {
        self.0.get()
    }

    /// Returns the leaf that carries this vector.
    pub(super) fn leaf_index(self) -> LeafIndex {
        // CALC: `self.0 / VECTORS_PER_LEAF`.
        self.0.shr::<{ VECTORS_PER_LEAF.ilog2() }, _>().cast()
    }

    /// Returns this vector's bit within its leaf.
    pub(super) const fn leaf_mask(self) -> LeafMask {
        LeafMask(1 << (self.0.get() % VECTORS_PER_LEAF))
    }

    /// Returns the subtree that carries this vector.
    pub(super) const fn subtree(self) -> Subtree {
        // INVARIANT: a shift of `1` leaves exactly one bit set.
        Subtree(1 << (self.0.get() / (VECTORS_PER_LEAF * LEAVES_PER_SUBTREE)))
    }

    /// Checks that this vector lies within a tree of `leaves` leaves.
    ///
    /// # Errors
    ///
    /// `EINVAL` if the vector lies beyond the last leaf such a tree implements.
    pub(super) const fn validate(self, leaves: LeafCount) -> Result {
        if self.0.get() >= leaves.vector_count() {
            return Err(EINVAL);
        }

        Ok(())
    }
}

impl From<Bounded<u32, 32>> for LeafMask {
    fn from(vectors: Bounded<u32, 32>) -> Self {
        Self(vectors.get())
    }
}

impl From<LeafMask> for Bounded<u32, 32> {
    fn from(vectors: LeafMask) -> Self {
        vectors.0.into()
    }
}

impl From<Bounded<u32, 32>> for SubtreeSet {
    fn from(subtrees: Bounded<u32, 32>) -> Self {
        Self(subtrees.get())
    }
}

impl From<SubtreeSet> for Bounded<u32, 32> {
    fn from(subtrees: SubtreeSet) -> Self {
        subtrees.0.into()
    }
}

impl From<GinVector> for Bounded<u32, TRIGGER_VECTOR_BITS> {
    fn from(vector: GinVector) -> Self {
        vector.0.extend()
    }
}

/// Clears the enables of the vectors set in `vectors` for `leaf` (`LEAF_EN_CLEAR`).
fn clear_leaf_enables(bar: Bar0<'_>, leaf: LeafIndex, vectors: LeafMask) {
    bar.write(
        NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_CLEAR::at(*leaf),
        NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_CLEAR::zeroed().with_vectors(vectors),
    );
}

/// Clears the `TOP` enables of every subtree in `serviced` (`TOP_EN_CLEAR`).
fn clear_top_enables(bar: Bar0<'_>, serviced: SubtreeSet) {
    bar.write_reg(NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_CLEAR::zeroed().with_subtrees(serviced));
}

/// Clears the pending vectors set in `vectors` for `leaf` (write-1-to-clear).
fn clear_leaf_pending(bar: Bar0<'_>, leaf: LeafIndex, vectors: LeafMask) {
    if !vectors.is_empty() {
        bar.write(
            NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF::at(*leaf),
            NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF::zeroed().with_vectors(vectors),
        );
    }
}

/// Returns every leaf a tree of `leaves` leaves implements.
fn implemented_leaves(leaves: LeafCount) -> impl Iterator<Item = LeafIndex> {
    (0..leaves.into_raw()).filter_map(LeafIndex::try_new)
}

/// The GIN CPU interrupt tree for a single PCIe function.
pub(super) struct Tree<'a> {
    /// Borrowed BAR0, through which every tree register is reached.
    bar: Bar0<'a>,
    /// Number of leaves this tree implements.
    leaves: LeafCount,
    /// The subtrees this tree enables and services.
    serviced: SubtreeSet,
    /// Method that rearms PCI interrupt delivery.
    rearm: PciIrqRearmMethod,
}

impl<'a> Tree<'a> {
    /// Creates a `Tree` for `chipset` covering `serviced`, with the rearm method that `msi_type`
    /// requires.
    ///
    /// Each subtree in `serviced` must have an allocated PCI vector and a registered handler.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `serviced` names a subtree this architecture does not implement.
    pub(super) fn new(
        bar: Bar0<'a>,
        chipset: Chipset,
        msi_type: MsiType,
        serviced: SubtreeSet,
    ) -> Result<Self> {
        let hal = cpu_interrupt_hal(chipset);
        let leaves = hal.leaf_count();

        if serviced.intersection(leaves.subtree_set()) != serviced {
            return Err(EINVAL);
        }

        Ok(Self {
            bar,
            leaves,
            serviced,
            rearm: hal.pci_irq_rearm_method(msi_type),
        })
    }

    /// Rearms PCI interrupt delivery to the CPU after servicing `subtree`, the one subtree the
    /// calling handler serves.
    ///
    /// A handler must call this before it returns, or it receives no further interrupts.
    pub(super) fn rearm_pci_irq(&self, subtree: Subtree) {
        self.rearm.rearm(self.bar, self.serviced, subtree);
    }

    /// Enables this tree's serviced subtrees (`TOP_EN_SET`).
    pub(super) fn enable_top(&self) {
        self.bar.write_reg(
            NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_TOP_EN_SET::zeroed().with_subtrees(self.serviced),
        );
    }

    /// Disables this tree's serviced subtrees (`TOP_EN_CLEAR`).
    pub(super) fn disable_top(&self) {
        clear_top_enables(self.bar, self.serviced);
    }

    /// Enables this tree's serviced subtrees until the returned guard drops.
    pub(super) fn enable_top_guarded(&self) -> TopEnableGuard<'a> {
        self.enable_top();

        TopEnableGuard {
            bar: self.bar,
            serviced: self.serviced,
        }
    }

    /// Enables the vectors set in `vectors` for `leaf` (`LEAF_EN_SET`).
    pub(super) fn enable_leaf(&self, leaf: LeafIndex, vectors: LeafMask) {
        self.bar.write(
            NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_SET::at(*leaf),
            NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_EN_SET::zeroed().with_vectors(vectors),
        );
    }

    /// Disables the vectors set in `vectors` for `leaf` (`LEAF_EN_CLEAR`).
    pub(super) fn disable_leaf(&self, leaf: LeafIndex, vectors: LeafMask) {
        clear_leaf_enables(self.bar, leaf, vectors);
    }

    /// Enables `vectors` for `leaf` until the returned guard drops.
    pub(super) fn enable_leaf_guarded(
        &self,
        leaf: LeafIndex,
        vectors: LeafMask,
    ) -> LeafEnableGuard<'a> {
        self.enable_leaf(leaf, vectors);

        LeafEnableGuard {
            bar: self.bar,
            leaf,
            vectors,
        }
    }

    /// Reads the vectors pending in `leaf`.
    pub(super) fn read_pending(&self, leaf: LeafIndex) -> LeafPending<'a> {
        let pending = self
            .bar
            .read(NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF::at(*leaf))
            .vectors();

        LeafPending {
            bar: self.bar,
            leaf,
            pending,
        }
    }

    /// Injects a software interrupt for `vector` via the trigger register.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `vector` lies outside this tree.
    // Only the interrupt self-test injects a software interrupt.
    #[expect(dead_code)]
    pub(super) fn trigger(&self, vector: GinVector) -> Result {
        vector.validate(self.leaves)?;
        self.bar.write_reg(
            NV_VIRTUAL_FUNCTION_PRIV_CPU_INTR_LEAF_TRIGGER::zeroed().with_vector(vector),
        );

        Ok(())
    }

    /// Disables every vector in every implemented leaf (`LEAF_EN_CLEAR`).
    ///
    /// This reaches vectors outside the subtrees nova-core services, so call it only during
    /// probe.
    pub(super) fn disable_all_leaves(&self) {
        for leaf in implemented_leaves(self.leaves) {
            self.disable_leaf(leaf, LeafMask::all());
        }
    }

    /// Clears every pending bit in every implemented leaf.
    ///
    /// Disables this tree's serviced subtrees at `TOP` and leaves them disabled, so a caller that
    /// needs delivery enables them itself. The pending bits cleared reach subtrees nova-core does
    /// not service.
    ///
    /// Call this only during probe. It must not run concurrently with an interrupt handler.
    pub(super) fn drain(&self) {
        self.disable_top();

        // A vector that latched while disabled is absent from `TOP`, so read every leaf.
        for leaf in implemented_leaves(self.leaves) {
            let pending = self.read_pending(leaf);
            if !pending.vectors().is_empty() {
                pending.clear();
            }
        }
    }
}

/// The vectors read pending from one leaf.
///
/// Holding one is the proof that the leaf was read, which is what [`Self::clear`] and
/// [`Self::clear_vectors`] require.
pub(super) struct LeafPending<'a> {
    bar: Bar0<'a>,
    leaf: LeafIndex,
    pending: LeafMask,
}

impl LeafPending<'_> {
    /// Returns the vectors that were pending.
    pub(super) fn vectors(&self) -> LeafMask {
        self.pending
    }

    /// Clears every vector that was pending, by writing its bits back (write-1-to-clear).
    pub(super) fn clear(&self) {
        self.clear_vectors(self.pending);
    }

    /// Clears the vectors set in `vectors` (write-1-to-clear), leaving every other pending bit
    /// set.
    pub(super) fn clear_vectors(&self, vectors: LeafMask) {
        clear_leaf_pending(self.bar, self.leaf, vectors);
    }
}

/// Keeps a leaf's vectors enabled for as long as it is held, and disables the same vectors on
/// drop.
pub(super) struct LeafEnableGuard<'a> {
    bar: Bar0<'a>,
    leaf: LeafIndex,
    vectors: LeafMask,
}

impl Drop for LeafEnableGuard<'_> {
    fn drop(&mut self) {
        clear_leaf_enables(self.bar, self.leaf, self.vectors);
    }
}

/// Keeps a tree's serviced subtrees enabled at `TOP` for as long as it is held, and disables them
/// on drop.
pub(super) struct TopEnableGuard<'a> {
    bar: Bar0<'a>,
    serviced: SubtreeSet,
}

impl Drop for TopEnableGuard<'_> {
    fn drop(&mut self) {
        clear_top_enables(self.bar, self.serviced);
    }
}
