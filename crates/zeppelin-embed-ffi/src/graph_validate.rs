//! Checked scalar contract conversions; no pointer dereference or graph admission.
use crate::{ZeErrorCode, ZeNodeId, ZeRelId};
use zeppelin_embed::property_graph::{NodeId, RelId};

impl From<NodeId> for ZeNodeId {
    fn from(id: NodeId) -> Self {
        Self {
            high: (id.get() >> 64) as u64,
            low: id.get() as u64,
        }
    }
}
impl From<RelId> for ZeRelId {
    fn from(id: RelId) -> Self {
        Self {
            high: (id.get() >> 64) as u64,
            low: id.get() as u64,
        }
    }
}
impl TryFrom<ZeNodeId> for NodeId {
    type Error = ZeErrorCode;
    fn try_from(id: ZeNodeId) -> Result<Self, Self::Error> {
        NodeId::new((u128::from(id.high) << 64) | u128::from(id.low))
            .map_err(|_| ZeErrorCode::ZeErrInvalidArgument)
    }
}
impl TryFrom<ZeRelId> for RelId {
    type Error = ZeErrorCode;
    fn try_from(id: ZeRelId) -> Result<Self, Self::Error> {
        RelId::new((u128::from(id.high) << 64) | u128::from(id.low))
            .map_err(|_| ZeErrorCode::ZeErrInvalidArgument)
    }
}

impl crate::ZeGraphRange {
    /// Checks arithmetic and containment without dereferencing caller memory.
    pub fn checked_range(self, elements: usize) -> Result<std::ops::Range<usize>, ZeErrorCode> {
        let end = self
            .start
            .checked_add(self.count)
            .ok_or(ZeErrorCode::ZeErrInvalidArgument)?;
        if end as usize > elements {
            return Err(ZeErrorCode::ZeErrInvalidArgument);
        }
        Ok(self.start as usize..end as usize)
    }
}
impl crate::ZeGraphValue {
    /// Validates this descriptor's fixed shape only. This is not pool,
    /// recursive list, pointer, UTF-8, property-content or parameter admission.
    pub fn validate_shape(&self) -> Result<(), ZeErrorCode> {
        use crate::{ZeGraphListKind as L, ZeGraphValueTag as V};
        let invalid = ZeErrorCode::ZeErrInvalidArgument;
        if self.abi_size as usize != std::mem::size_of::<Self>() || self.abi_reserved != 0 {
            return Err(invalid);
        }
        let is_list = self.tag == V::ZeGraphValueList as u32;
        let is_entity = self.tag == V::ZeGraphValueNode as u32
            || self.tag == V::ZeGraphValueRelationship as u32;
        let is_range = is_list || self.tag == V::ZeGraphValueString as u32;
        if self.tag > V::ZeGraphValueList as u32
            || (!is_list && self.list_kind != 0)
            || (!is_entity && self.entity_index != 0)
            || (self.tag != V::ZeGraphValueBool as u32 && self.boolean != 0)
            || (self.tag == V::ZeGraphValueBool as u32 && self.boolean > 1)
            || (self.tag != V::ZeGraphValueI64 as u32 && self.integer != 0)
            || (self.tag != V::ZeGraphValueF64 as u32 && self.floating.to_bits() != 0)
            || (!is_range && (self.range.start != 0 || self.range.count != 0))
            || (is_list
                && (self.list_kind > L::ZeGraphListEmpty as u32
                    || self.range.count > 524_288
                    || (self.list_kind == L::ZeGraphListEmpty as u32 && self.range.count != 0)))
        {
            return Err(invalid);
        }
        self.range.checked_range(usize::MAX)?;
        Ok(())
    }
    /// Additionally rejects query-only outer tags for stored properties.
    /// Element homogeneity/content is checked by the later pool marshaller.
    pub fn validate_stored_shape(&self) -> Result<(), ZeErrorCode> {
        use crate::{ZeGraphListKind as L, ZeGraphValueTag as V};
        self.validate_shape()?;
        if self.tag == V::ZeGraphValueNull as u32
            || self.tag == V::ZeGraphValueNode as u32
            || self.tag == V::ZeGraphValueRelationship as u32
            || (self.tag == V::ZeGraphValueList as u32
                && self.list_kind == L::ZeGraphListQuery as u32)
        {
            return Err(ZeErrorCode::ZeErrInvalidArgument);
        }
        Ok(())
    }
}

fn exact_header<T>(size: u32, reserved: u32) -> Result<(), ZeErrorCode> {
    if size as usize != std::mem::size_of::<T>() || reserved != 0 {
        Err(ZeErrorCode::ZeErrInvalidArgument)
    } else {
        Ok(())
    }
}
impl crate::ZeGraphEndpoint {
    /// Validates fixed representation only; local index existence, prior node
    /// creation and live-store endpoint resolution belong to batch admission.
    pub fn validate_shape(&self) -> Result<(), ZeErrorCode> {
        exact_header::<Self>(self.abi_size, self.abi_reserved)?;
        let zero = self.node.high == 0 && self.node.low == 0;
        let valid = match self.kind {
            0 => zero && self.local_item == 0,
            1 => !zero && self.local_item == 0,
            2 => zero && self.local_item < 16_384,
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(ZeErrorCode::ZeErrInvalidArgument)
        }
    }
}

impl crate::ZeGraphBatchItem {
    /// Checks scalar precondition representation without dereferencing images,
    /// interpreting name bytes or resolving current key/endpoint state.
    pub fn validate_shape(&self) -> Result<(), ZeErrorCode> {
        exact_header::<Self>(self.abi_size, self.abi_reserved)?;
        self.namespace_name.checked_range(usize::MAX)?;
        self.key.checked_range(usize::MAX)?;
        self.source.validate_shape()?;
        self.target.validate_shape()?;
        let invalid = ZeErrorCode::ZeErrInvalidArgument;
        let node_zero = self.expected_node.high == 0 && self.expected_node.low == 0;
        let rel_zero = self.expected_relationship.high == 0 && self.expected_relationship.low == 0;
        if self.entity_kind > 1
            || self.operation > 3
            || self.revision == 0
            || self.reserved != 0
            || self.has_image > 1
            || self.delete_mode > 1
            || (self.has_image == 0 && self.image != 0)
        {
            return Err(invalid);
        }
        let delete = self.operation == 2;
        let incarnation = self.operation == 1 || delete;
        if self.has_image != u32::from(!delete)
            || (!delete && self.delete_mode != 0)
            || (self.entity_kind == 1 && self.delete_mode != 0)
            || (self.operation != 3 && self.expected_deletion_revision != 0)
            || (self.operation == 3 && self.expected_deletion_revision == 0)
            || (incarnation
                && if self.entity_kind == 0 {
                    node_zero || !rel_zero
                } else {
                    rel_zero || !node_zero
                })
            || (!incarnation && (!node_zero || !rel_zero))
        {
            return Err(invalid);
        }
        let endpoints = self.entity_kind == 1 && !delete;
        if (endpoints && (self.source.kind == 0 || self.target.kind == 0))
            || (!endpoints && (self.source.kind != 0 || self.target.kind != 0))
        {
            return Err(invalid);
        }
        Ok(())
    }
}

impl crate::ZeGraphWorkLimit {
    /// Validates a caller-tightened category against existing runtime defaults;
    /// this does not reserve memory or report that any work was performed.
    pub fn validate_shape(&self) -> Result<(), ZeErrorCode> {
        use zeppelin_embed::property_graph::query::runtime::{RuntimeLimits, WorkKind as W};
        exact_header::<Self>(self.abi_size, self.abi_reserved)?;
        if self.reserved != 0 {
            return Err(ZeErrorCode::ZeErrInvalidArgument);
        }
        let kind = match self.kind {
            0 => W::OperatorRows,
            1 => W::AdjacencyEntries,
            2 => W::Expressions,
            3 => W::HashProbes,
            4 => W::CompletedRows,
            5 => W::CompletedBytes,
            6 => W::PreparedPayloadBytes,
            7 => W::CompletedAbiBytes,
            8 => W::VectorCoordinates,
            9 => W::VectorBytes,
            10 => W::LexicalPostings,
            11 => W::LexicalBlocks,
            12 => W::SearchInvocations,
            13 => W::Lookups,
            14 => W::Scans,
            15 => W::Paths,
            16 => W::RowsIn,
            17 => W::RowsOut,
            18 => W::JoinProbes,
            19 => W::GroupKeys,
            20 => W::EligibilityEntries,
            21 => W::CopiedBytes,
            _ => return Err(ZeErrorCode::ZeErrInvalidArgument),
        };
        RuntimeLimits::default()
            .with_limit(kind, self.limit)
            .map(|_| ())
            .map_err(|_| ZeErrorCode::ZeErrInvalidArgument)
    }
}
impl crate::ZeGraphQueryLimits {
    /// Checks scalar shape and pointer/count arithmetic without dereferencing.
    /// The marshaller still validates each accessible row and duplicate kinds.
    pub fn validate_shape(&self) -> Result<(), ZeErrorCode> {
        exact_header::<Self>(self.abi_size, self.abi_reserved)?;
        if self.reserved != 0
            || self.has_query_bytes > 1
            || (self.has_query_bytes == 0 && self.query_bytes != 0)
            || self.query_bytes > 24 * 1024 * 1024
            || self.work_count > 22
        {
            return Err(ZeErrorCode::ZeErrInvalidArgument);
        }
        array_shape(self.work, self.work_count)
    }
}
fn array_shape<T>(pointer: *const T, count: usize) -> Result<(), ZeErrorCode> {
    if count == 0 {
        return Ok(());
    }
    if pointer.is_null()
        || !(pointer as usize).is_multiple_of(std::mem::align_of::<T>())
        || count
            .checked_mul(std::mem::size_of::<T>())
            .is_none_or(|size| size > isize::MAX as usize)
    {
        return Err(ZeErrorCode::ZeErrInvalidArgument);
    }
    Ok(())
}

macro_rules! descriptor_headers {
    ($($name:ident),* $(,)?) => {$ (
        impl crate::$name {
            /// Validates only the exact version-one header and array stride.
            /// This is not tag, pointer, contents, ownership or semantic admission.
            pub fn validate_header(&self) -> Result<(), ZeErrorCode> {
                exact_header::<Self>(self.abi_size, self.abi_reserved)
            }
        }
    )* };
}
descriptor_headers!(
    ZeGraphValue,
    ZeGraphControl,
    ZeGraphProperty,
    ZeGraphNode,
    ZeGraphRelationship,
    ZeGraphValuePool,
    ZeGraphEndpoint,
    ZeGraphBatchItem,
    ZeGraphBatchRequest,
    ZeGraphExpression,
    ZeGraphProjection,
    ZeGraphSortKey,
    ZeGraphParameter,
    ZeGraphParameterValue,
    ZeGraphSearchOptions,
    ZeGraphSearch,
    ZeGraphOperator,
    ZeGraphMutation,
    ZeGraphPlan,
    ZeGraphQueryOptions,
    ZeGraphQueryRequest,
    ZeGraphOpenRequest,
    ZeGraphCypherRequest,
    ZeGraphGetNodesRequest,
    ZeGraphGetRelsRequest,
    ZeGraphReceipt,
    ZeGraphColumn,
    ZeGraphDiagnostic,
    ZeGraphWorkCounter,
    ZeGraphSearchReport,
    ZeGraphResponse,
    ZeGraphWorkLimit,
    ZeGraphQueryLimits,
    ZeGraphCompileLimits,
);

impl crate::ZeGraphOptionalIndex {
    /// Checks presence independently of index zero; checks no referent or scope.
    pub fn validate_shape(&self) -> Result<(), ZeErrorCode> {
        if self.present > 1 || (self.present == 0 && self.index != 0) {
            Err(ZeErrorCode::ZeErrInvalidArgument)
        } else {
            Ok(())
        }
    }
}
impl crate::ZeGraphSearch {
    /// Validates only scalar shape. Evaluated bounds, expression scopes,
    /// same-view eligibility ownership and search options require admission.
    pub fn validate_shape(&self) -> Result<(), ZeErrorCode> {
        self.validate_header()?;
        for index in [
            self.vector,
            self.text,
            self.eligible_set,
            self.window,
            self.vector_distance_slot,
            self.lexical_score_slot,
        ] {
            index.validate_shape()?;
        }
        if self.kind > 2
            || self.call_id >= 8
            || self.reserved != 0
            || self.has_tier > 1
            || self.tier > 3
            || (self.has_tier == 0 && self.tier != 0)
            || self.vector.present != u32::from(self.kind != 1)
            || self.text.present != u32::from(self.kind != 0)
            || (self.kind == 1 && self.has_tier != 0)
            || (self.kind != 2
                && (self.vector_distance_slot.present != 0 || self.lexical_score_slot.present != 0))
        {
            return Err(ZeErrorCode::ZeErrInvalidArgument);
        }
        Ok(())
    }
}

impl crate::ZeGraphCompileLimits {
    /// Checks the existing textual profile ceilings; zero remains explicit.
    /// ZE-69 must pass these same limits into the outer compiler.
    pub fn validate_shape(&self) -> Result<(), ZeErrorCode> {
        self.validate_header()?;
        if self.text_bytes > 65_536
            || self.tokens > 8192
            || self.ast_nodes > 4096
            || self.depth > 64
            || self.parameters > 256
            || self.columns > 256
            || self.list_depth > 16
            || self.path_hops > 16
        {
            Err(ZeErrorCode::ZeErrInvalidArgument)
        } else {
            Ok(())
        }
    }
}
