// SPDX-License-Identifier: GPL-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

//! Vector addressing in the GIN CPU interrupt tree.
//!
//! A vector's number fixes where it latches: leaf `vector / 32` at bit `vector % 32`, and that
//! leaf belongs to subtree `vector / 64`. The types here keep those three views apart, so a leaf
//! index, a set of vectors within one leaf, and a `TOP` bit cannot stand in for one another.

use kernel::{
    num::Bounded,
    prelude::*, //
};

use crate::num;

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
pub(super) struct Subtree(u32);

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
pub(super) struct SubtreeSet(u32);

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
