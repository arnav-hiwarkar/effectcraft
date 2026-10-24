//! Shared cache byte reporting and budget control.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

/// Cache classification metadata. Variants do not define an eviction order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Gpu,
    Footage,
    Layer,
    Preview,
}

/// Reports a cache's existing accounting and preserves its budget behavior.
pub trait Budgeted {
    /// Bytes in the cache's accounted entries.
    ///
    /// This is not RSS, allocator capacity, or all memory retained by outside `Arc`s.
    /// Counts from different caches are not guaranteed to be additive.
    fn bytes(&self) -> usize;

    /// Apply a budget using the cache's existing timing and retention exceptions.
    /// The reported bytes may still exceed the budget after this call.
    fn set_budget(&self, bytes: usize);

    /// The cache's classification, independent of eviction policy.
    fn tier(&self) -> Tier;
}
