use super::GraphReadView;
use crate::property_graph::catalog::{Symbol, SymbolKind};
use crate::property_graph::query::runtime::{RetainedView, RuntimeContext};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::records::{RecordShape, verify_fence_entry, verify_record};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::{
    TreeKind,
    directory::{FenceKey, TreeError, TreeResources, lookup_entry, lookup_fence_entry},
};
use crate::property_graph::{ApplicationKey, EntityId, EntityKind, GraphName};

impl<'s, 'lease, 'm, 'g> GraphReadView<'s, 'lease, 'm, 'g> {
    pub(crate) fn validate_expression_owner(
        &self,
        runtime: &RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        self.lease
            .check_active()
            .map_err(crate::property_graph::query::runtime::RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        if !std::ptr::eq(runtime.view(), self.lease.query_view())
            || runtime.identity() != self.source.runtime()
            || !std::ptr::eq(runtime.memory(), self.source.memory())
            || !self.catalog.owns(self.source)
        {
            return Err(TreeError::Invalid("native expression owner mismatch"));
        }
        Ok(())
    }

    pub(crate) fn expression_symbol(
        &self,
        kind: SymbolKind,
        name: GraphName<'_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<Symbol>, TreeError> {
        self.catalog.lookup(kind, name, resources)
    }

    pub(crate) fn expression_symbol_name(
        &self,
        symbol: Symbol,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<GraphName<'s>>, TreeError> {
        self.catalog.name(symbol, resources)
    }

    /// Resolves one exact kind/namespace/application key in this admitted view.
    /// The permanent fence is only an index: the authoritative entity record
    /// and, for relationships, both endpoints must still be live and agree with
    /// the fence's complete installing provenance.
    pub(crate) fn lookup_application_key(
        &self,
        key: ApplicationKey<'_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<EntityId>, TreeError> {
        self.lease
            .check_active()
            .map_err(crate::property_graph::query::runtime::RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        let Some(Symbol::Namespace(namespace_id)) =
            self.expression_symbol(SymbolKind::Namespace, key.namespace(), resources)?
        else {
            return Ok(None);
        };
        let roots = self.lease.bundle().roots();
        let root = roots.directory(TreeKind::KeyFences)?;
        let fence_key = FenceKey::new(key.kind(), namespace_id, key.key().as_str())?;
        let Some(entry) = lookup_fence_entry(self.source, root, fence_key, resources)? else {
            return Ok(None);
        };
        let fence = verify_fence_entry(
            self.source,
            root,
            entry,
            self.catalog,
            self.lease.bundle().document(),
            resources,
        )?;
        let fence_fields = fence.provenance().fields_with_key(Some(key), resources)?;
        if fence.is_deleted() {
            return Ok(None);
        }
        let fence_canonical = fence.canonical_bytes().ok_or(TreeError::Invalid(
            "live application fence lacks canonical bytes",
        ))?;
        let incarnation = fence.incarnation();
        let (revision, fields, canonical) = match incarnation {
            EntityId::Node(node) if key.kind() == EntityKind::Node => {
                let Some(record) = self.lookup_node(node, resources)? else {
                    return Err(TreeError::Invalid(
                        "live node fence lacks authoritative record",
                    ));
                };
                (
                    record.record().revision(),
                    record
                        .record()
                        .provenance()
                        .fields_with_key(Some(key), resources)?,
                    record.record().canonical_bytes(),
                )
            }
            EntityId::Relationship(relationship) if key.kind() == EntityKind::Relationship => {
                let roots = self.lease.bundle().roots();
                let Some(entry) = lookup_entry(
                    self.source,
                    roots.directory(TreeKind::Relationships)?,
                    &relationship.get().to_le_bytes(),
                    resources,
                )?
                else {
                    return Err(TreeError::Invalid(
                        "live relationship fence lacks authoritative record",
                    ));
                };
                let payload = PayloadRef::decode(entry.value())?;
                let record = verify_record(
                    PayloadSlice::new(
                        self.source,
                        roots.store(),
                        entry.creation_generation(),
                        payload,
                    ),
                    incarnation,
                    self.catalog,
                    self.lease.bundle().document(),
                    resources,
                )?;
                let RecordShape::Relationship { source, target, .. } = record.shape() else {
                    return Err(TreeError::Invalid("application key relationship role"));
                };
                let reader = super::super::adjacency::NativeGraphReader::new(
                    self.source,
                    roots,
                    self.lease.bundle().sequence(),
                    self.catalog,
                    self.lease.bundle().document(),
                );
                let visible = reader.endpoint_live(source, resources)?
                    && reader.endpoint_live(target, resources)?;
                let revision = record.revision();
                let fields = record.provenance().fields_with_key(Some(key), resources)?;
                let canonical = record.canonical_bytes();
                if revision != fence.revision()
                    || fields != fence_fields
                    || canonical.compare(fence_canonical, resources)?.is_ne()
                {
                    return Err(TreeError::Invalid(
                        "application key record differs from fence",
                    ));
                }
                if !visible {
                    return Ok(None);
                }
                (revision, fields, canonical)
            }
            _ => return Err(TreeError::Invalid("application key kind mismatch")),
        };
        if revision != fence.revision()
            || fields != fence_fields
            || canonical.compare(fence_canonical, resources)?.is_ne()
        {
            return Err(TreeError::Invalid(
                "application key record differs from fence",
            ));
        }
        Ok(Some(incarnation))
    }
}
