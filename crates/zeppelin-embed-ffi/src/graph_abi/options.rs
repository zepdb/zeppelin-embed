//! Request-local interpretation and hard-limit tightening.
use super::{invalid, read_exact};
use crate::{error::FfiError, marshal, *};
use zeppelin_embed::epoch::EmbeddingTower;
use zeppelin_embed::property_graph::query::completed::GraphQueryOptions;
use zeppelin_embed::property_graph::query::runtime::{RuntimeLimits, WorkKind};
const WORK: [WorkKind; 22] = [
    WorkKind::OperatorRows,
    WorkKind::AdjacencyEntries,
    WorkKind::Expressions,
    WorkKind::HashProbes,
    WorkKind::CompletedRows,
    WorkKind::CompletedBytes,
    WorkKind::PreparedPayloadBytes,
    WorkKind::CompletedAbiBytes,
    WorkKind::VectorCoordinates,
    WorkKind::VectorBytes,
    WorkKind::LexicalPostings,
    WorkKind::LexicalBlocks,
    WorkKind::SearchInvocations,
    WorkKind::Lookups,
    WorkKind::Scans,
    WorkKind::Paths,
    WorkKind::RowsIn,
    WorkKind::RowsOut,
    WorkKind::JoinProbes,
    WorkKind::GroupKeys,
    WorkKind::EligibilityEntries,
    WorkKind::CopiedBytes,
];
pub(super) fn limits(
    pointer: *const ZeGraphQueryLimits,
    mut options: GraphQueryOptions,
) -> Result<GraphQueryOptions, FfiError> {
    if pointer.is_null() {
        return Ok(options);
    }
    let raw = read_exact(pointer, |v| v.abi_size, "query limits")?;
    raw.validate_header()
        .map_err(|_| invalid("query limits header"))?;
    if raw.reserved != 0
        || raw.has_query_bytes > 1
        || (raw.has_query_bytes == 0 && raw.query_bytes != 0)
        || raw.work_count > 22
    {
        return Err(invalid("query limits fields"));
    }
    let memory = if raw.has_query_bytes == 1 {
        usize::try_from(raw.query_bytes).map_err(|_| invalid("query memory overflow"))?
    } else {
        options.memory_limit()
    };
    let mut work = RuntimeLimits::default();
    let mut seen = [false; 22];
    for limit in marshal::read_slice(raw.work, raw.work_count).map_err(|e| invalid(e.0))? {
        limit
            .validate_header()
            .map_err(|_| invalid("work limit header"))?;
        let kind = WORK
            .get(limit.kind as usize)
            .ok_or_else(|| invalid("unknown work limit"))?;
        let seen = seen
            .get_mut(limit.kind as usize)
            .ok_or_else(|| invalid("work limit index"))?;
        if *seen || limit.reserved != 0 {
            return Err(invalid("duplicate or reserved work limit"));
        }
        *seen = true;
        work = work
            .with_limit(*kind, limit.limit)
            .map_err(|_| invalid("work limit widens hard maximum"))?;
    }
    options = options
        .with_limits(memory, work)
        .map_err(|_| invalid("query memory widens hard maximum"))?;
    Ok(options)
}
pub(super) fn query(
    pointer: *const ZeGraphQueryOptions,
    document: Option<&EmbeddingTower>,
    options: GraphQueryOptions,
) -> Result<GraphQueryOptions, FfiError> {
    if pointer.is_null() {
        return Ok(options);
    }
    let raw = read_exact(pointer, |v| v.abi_size, "query options")?;
    raw.validate_header()
        .map_err(|_| invalid("query options header"))?;
    if raw.alignment_digest.count > 65_536 {
        return Err(invalid("alignment digest exceeds 65536 bytes"));
    }
    let alignment = marshal::read_slice(raw.alignment_digest.data, raw.alignment_digest.count)
        .map_err(|e| invalid(e.0))?;
    if raw.query_tower.is_null() {
        if !alignment.is_empty() {
            return Err(invalid("alignment requires a query tower"));
        }
    } else {
        if raw
            .query_tower
            .align_offset(std::mem::align_of::<ZeEmbeddingTower>())
            != 0
        {
            return Err(invalid("query tower is misaligned"));
        }
        let declaration = marshal::read_value(raw.query_tower);
        let tower = crate::parse_tower(declaration)?;
        let document = document.ok_or_else(|| {
            FfiError::new(
                ZeErrorCode::ZeErrNoVectorSpace,
                "query tower supplied without document vector space",
            )
        })?;
        // The caller explicitly attests an asymmetric pairing by naming its
        // alignment artifact. Dimension alone never establishes compatibility.
        if tower.dims != document.dims
            || tower.normalization != document.normalization
            || (&tower != document && alignment.is_empty())
        {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrEpochMismatch,
                "query/document towers require compatible geometry and an explicit alignment digest",
            ));
        }
    }
    limits(raw.limits, options)
}
