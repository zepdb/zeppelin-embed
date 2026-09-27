//! Logical catalog state. Publication and durable store admission have separate owners.

use super::GraphName;

mod codec;
mod interpretation;
mod rules;
mod work;
pub use codec::CatalogImage;
pub use interpretation::{CatalogDeclaration, DocumentDeclaration, GraphInterpretation};
pub use rules::{OnDelete, RelationshipRule, RelationshipRules};

/// Invalid catalog identity or symbol allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogError {
    /// Zero is reserved for an unallocated symbol.
    ZeroSymbol,
    /// The symbol domain has no unused positive integer remaining.
    SymbolOverflow,
    /// Caller-provided descriptor allowance is insufficient.
    Capacity,
    /// Fallible reservation failed.
    Allocation,
    /// A reconstruction repeats one symbol or one exact name in a domain.
    Duplicate,
    /// A persisted symbol exceeds its retained allocator high-water.
    HighWater,
    /// Work was cancelled before completing this operation.
    Cancelled,
    /// Required catalog framing or graph interpretation is unsupported.
    Unsupported,
    /// Persisted interpretation differs from the explicit caller declaration.
    InterpretationMismatch,
    /// Optional vector data has no declared document space.
    NoEmbeddingSpace,
    /// A declared document space has invalid dimensions or excessive metadata.
    InvalidEmbedding,
    /// Persisted catalog bytes are malformed or truncated.
    Malformed,
    /// The complete logical catalog checksum differs.
    Checksum,
    /// The catalog belongs to a different storage-owned store incarnation.
    StoreMismatch,
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "graph catalog: {self:?}")
    }
}
impl std::error::Error for CatalogError {}

macro_rules! symbol {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);
        impl $name {
            /// Validates all 64 bits of an allocated symbol.
            pub const fn new(value: u64) -> Result<Self, CatalogError> {
                if value == 0 {
                    Err(CatalogError::ZeroSymbol)
                } else {
                    Ok(Self(value))
                }
            }
            /// Returns the complete symbol value.
            pub const fn get(self) -> u64 {
                self.0
            }
            /// Allocates the next symbol without wrapping or reusing zero.
            pub fn next(self) -> Result<Self, CatalogError> {
                Self::new(self.0.checked_add(1).ok_or(CatalogError::SymbolOverflow)?)
            }
        }
    };
}
symbol!(
    LabelId,
    "A checked label symbol, distinct from every other namespace."
);
symbol!(RelTypeId, "A checked relationship-type symbol.");
symbol!(PropertyKeyId, "A checked property-name symbol.");
symbol!(NamespaceId, "A checked application-key namespace symbol.");

/// Independent, append-only symbol domains.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum SymbolKind {
    /// Node labels.
    Label = 1,
    /// Relationship types.
    RelationshipType = 2,
    /// Property names.
    Property = 3,
    /// Application-key namespaces.
    Namespace = 4,
}

/// A symbol retains its domain at generic catalog/codec seams.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Symbol {
    /// Node label.
    Label(LabelId),
    /// Relationship type.
    RelationshipType(RelTypeId),
    /// Property name.
    Property(PropertyKeyId),
    /// Application namespace.
    Namespace(NamespaceId),
}
impl Symbol {
    /// Returns the symbol's domain.
    pub const fn kind(self) -> SymbolKind {
        match self {
            Self::Label(_) => SymbolKind::Label,
            Self::RelationshipType(_) => SymbolKind::RelationshipType,
            Self::Property(_) => SymbolKind::Property,
            Self::Namespace(_) => SymbolKind::Namespace,
        }
    }
    /// Returns the full symbol value.
    pub const fn get(self) -> u64 {
        match self {
            Self::Label(v) => v.get(),
            Self::RelationshipType(v) => v.get(),
            Self::Property(v) => v.get(),
            Self::Namespace(v) => v.get(),
        }
    }
    /// Checks a persisted domain-scoped symbol.
    pub fn new(kind: SymbolKind, value: u64) -> Result<Self, CatalogError> {
        Ok(match kind {
            SymbolKind::Label => Self::Label(LabelId::new(value)?),
            SymbolKind::RelationshipType => Self::RelationshipType(RelTypeId::new(value)?),
            SymbolKind::Property => Self::Property(PropertyKeyId::new(value)?),
            SymbolKind::Namespace => Self::Namespace(NamespaceId::new(value)?),
        })
    }
}

/// Inclusive allocator high-waters; zero means never allocated. Gaps are retained.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SymbolHighWaters {
    /// Largest allocated label.
    pub label: u64,
    /// Largest allocated relationship type.
    pub relationship_type: u64,
    /// Largest allocated property name.
    pub property: u64,
    /// Largest allocated application namespace.
    pub namespace: u64,
}
impl SymbolHighWaters {
    /// Returns the allocator high-water for this domain.
    pub const fn get(self, kind: SymbolKind) -> u64 {
        match kind {
            SymbolKind::Label => self.label,
            SymbolKind::RelationshipType => self.relationship_type,
            SymbolKind::Property => self.property,
            SymbolKind::Namespace => self.namespace,
        }
    }
    fn set(&mut self, symbol: Symbol) {
        match symbol {
            Symbol::Label(v) => self.label = v.get(),
            Symbol::RelationshipType(v) => self.relationship_type = v.get(),
            Symbol::Property(v) => self.property = v.get(),
            Symbol::Namespace(v) => self.namespace = v.get(),
        }
    }
}

/// One borrowed name assignment. Name backing stays charged to its caller/lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SymbolEntry<'a> {
    /// Checked domain and identity.
    pub symbol: Symbol,
    /// Exact name, with no case folding or Unicode normalization.
    pub name: GraphName<'a>,
}

/// Bounded working dictionary with borrowed names and fixed descriptor capacity.
/// This is optional staging/reconstruction scratch, not a mandatory resident store
/// catalog or a copy-per-write design. Durable pages and publication are later-owned.
#[derive(Debug)]
pub struct SymbolCatalog<'a> {
    entries: Vec<SymbolEntry<'a>>,
    limit: usize,
    high_waters: SymbolHighWaters,
}
impl<'a> SymbolCatalog<'a> {
    /// Reserves descriptors fallibly inside an allowance already reserved by the
    /// caller from the shared budget. Borrowed name bytes are charged separately.
    pub fn reconstruct(
        entries: &[SymbolEntry<'a>],
        high_waters: SymbolHighWaters,
        capacity: usize,
        byte_allowance: usize,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<Self, CatalogError> {
        checkpoint()?;
        let bytes = capacity
            .checked_mul(std::mem::size_of::<SymbolEntry<'a>>())
            .ok_or(CatalogError::Capacity)?;
        if entries.len() > capacity || bytes > byte_allowance {
            return Err(CatalogError::Capacity);
        }
        let mut owned = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let reserved = crate::allocation_audit::attributed(|| owned.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let reserved = owned.try_reserve_exact(capacity);
        reserved.map_err(|_| CatalogError::Allocation)?;
        if owned
            .capacity()
            .checked_mul(std::mem::size_of::<SymbolEntry<'a>>())
            .is_none_or(|bytes| bytes > byte_allowance)
        {
            return Err(CatalogError::Capacity);
        }
        for chunk in entries.chunks(work::CHUNK / std::mem::size_of::<SymbolEntry<'a>>()) {
            checkpoint()?;
            owned.extend_from_slice(chunk);
        }
        let mut result = Self {
            entries: owned,
            limit: capacity,
            high_waters,
        };
        result.validate(checkpoint)?;
        Ok(result)
    }
    fn validate(
        &mut self,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<(), CatalogError> {
        checkpoint()?;
        work::sort(&mut self.entries, checkpoint, |left, right, checkpoint| {
            checkpoint()?;
            Ok((left.symbol.kind(), left.symbol.get())
                .cmp(&(right.symbol.kind(), right.symbol.get())))
        })?;
        let mut previous: Option<Symbol> = None;
        for entry in &self.entries {
            checkpoint()?;
            if previous == Some(entry.symbol) {
                return Err(CatalogError::Duplicate);
            }
            if entry.symbol.get() > self.high_waters.get(entry.symbol.kind()) {
                return Err(CatalogError::HighWater);
            }
            previous = Some(entry.symbol);
        }
        work::sort(&mut self.entries, checkpoint, |left, right, checkpoint| {
            checkpoint()?;
            let order = left.symbol.kind().cmp(&right.symbol.kind());
            if order != std::cmp::Ordering::Equal {
                Ok(order)
            } else {
                work::compare_bytes(
                    left.name.as_str().as_bytes(),
                    right.name.as_str().as_bytes(),
                    checkpoint,
                )
            }
        })?;
        let mut previous: Option<(SymbolKind, GraphName<'a>)> = None;
        for entry in &self.entries {
            checkpoint()?;
            let key = (entry.symbol.kind(), entry.name);
            if let Some((kind, name)) = previous
                && kind == key.0
                && work::compare_bytes(
                    name.as_str().as_bytes(),
                    entry.name.as_str().as_bytes(),
                    checkpoint,
                )? == std::cmp::Ordering::Equal
            {
                return Err(CatalogError::Duplicate);
            }
            previous = Some(key);
        }
        Ok(())
    }
    /// Returns exact retained descriptor capacity bytes, including unused slots.
    pub fn allocated_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<SymbolEntry<'a>>()
    }
    /// Returns preserved inclusive high-waters, including domains with no rows.
    pub const fn high_waters(&self) -> SymbolHighWaters {
        self.high_waters
    }
    /// Exposes complete assignments. Private insertion order has no logical meaning.
    pub fn entries(&self) -> &[SymbolEntry<'a>] {
        &self.entries
    }
    /// Resolves an exact name in one domain without creating an unused symbol.
    pub fn lookup(
        &self,
        kind: SymbolKind,
        name: GraphName<'_>,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<Option<Symbol>, CatalogError> {
        checkpoint()?;
        for entry in &self.entries {
            checkpoint()?;
            if entry.symbol.kind() == kind
                && work::compare_bytes(
                    entry.name.as_str().as_bytes(),
                    name.as_str().as_bytes(),
                    checkpoint,
                )? == std::cmp::Ordering::Equal
            {
                return Ok(Some(entry.symbol));
            }
        }
        Ok(None)
    }
    /// Resolves a checked symbol, keeping domains distinct.
    pub fn name(
        &self,
        symbol: Symbol,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<Option<GraphName<'a>>, CatalogError> {
        checkpoint()?;
        for entry in &self.entries {
            checkpoint()?;
            if entry.symbol == symbol {
                return Ok(Some(entry.name));
            }
        }
        Ok(None)
    }
    /// Interns only a required name into private staging. An existing name neither
    /// advances a high-water nor consumes capacity, even at allocator exhaustion.
    pub fn intern(
        &mut self,
        kind: SymbolKind,
        name: GraphName<'a>,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<Symbol, CatalogError> {
        checkpoint()?;
        match self.lookup(kind, name, checkpoint)? {
            Some(symbol) => Ok(symbol),
            None => {
                if self.entries.len() >= self.limit {
                    return Err(CatalogError::Capacity);
                }
                let symbol = Symbol::new(
                    kind,
                    self.high_waters
                        .get(kind)
                        .checked_add(1)
                        .ok_or(CatalogError::SymbolOverflow)?,
                )?;
                checkpoint()?;
                self.entries.push(SymbolEntry { symbol, name });
                self.high_waters.set(symbol);
                Ok(symbol)
            }
        }
    }
}

#[cfg(all(test, feature = "allocation-audit"))]
#[allow(clippy::unwrap_used)]
mod allocation_tests {
    use super::*;

    #[test]
    fn catalog_descriptor_reservation_accounts_every_retained_byte() {
        let (catalog, audit) = crate::allocation_audit::audit_engine_path(|| {
            SymbolCatalog::reconstruct(&[], SymbolHighWaters::default(), 8, 4096, &mut || Ok(()))
                .unwrap()
        });
        assert_eq!(audit.unattributed_bytes, 0);
        assert_eq!(audit.attributed_bytes, catalog.allocated_bytes() as u64);
        assert_eq!(audit.allocations, 1);
        let mut catalog = catalog;
        let (_, audit) = crate::allocation_audit::audit_engine_path(|| {
            let name = GraphName::new("unchanged\0é").unwrap();
            let symbol = catalog
                .intern(SymbolKind::Label, name, &mut || Ok(()))
                .unwrap();
            assert_eq!(catalog.name(symbol, &mut || Ok(())).unwrap(), Some(name));
            assert_eq!(
                catalog
                    .lookup(SymbolKind::Label, name, &mut || Ok(()))
                    .unwrap(),
                Some(symbol)
            );
        });
        assert_eq!(audit.allocations, 0);
        let (rejected, audit) = crate::allocation_audit::audit_engine_path(|| {
            SymbolCatalog::reconstruct(&[], SymbolHighWaters::default(), 8, 0, &mut || Ok(()))
        });
        assert!(matches!(rejected, Err(CatalogError::Capacity)));
        assert_eq!(audit.allocations, 0);
    }
}
