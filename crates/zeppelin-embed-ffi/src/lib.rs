//! Stable C ABI for the Zeppelin embedded search engine.
//!
//! Every exported function catches unwinding panics. The crate deliberately
//! does not install a panic hook because a hook is process-global policy owned
//! by the embedding host.

#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unwrap_used,
    unsafe_op_in_unsafe_fn
)]
#![warn(missing_docs)]

mod abi;
mod error;
mod marshal;
mod registry;
mod slots;
mod sync;
#[cfg(feature = "text")]
mod text_registry;

use std::any::Any;
use std::ffi::c_char;
use std::mem::{align_of, size_of};
use std::path::Path;
use std::time::Duration;

use error::FfiError;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, EpochIdentity, Normalization,
    StoreEpoch,
};
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::fusion::{FusionMethod, HybridQuery, RuleSignals};
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{
    AccessMode, CancelToken, Deadline, DocumentFields, DocumentScanCursor, DocumentScanRequest,
    DocumentScanSource, GraphSearchOptions, MAX_DOCUMENT_SCAN_LIMIT, OpenOptions, QueryControl,
    ScanOrder, SearchOptions, SearchTier, Store, StoreState, StoredDocument,
};
use zeppelin_embed::meta::{
    ColumnDefinition, ColumnId, ColumnType, Predicate, PredicateValue, RangeBound, RangePredicate,
    Schema,
};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus};

pub use abi::*;

macro_rules! ffi_entry {
    ($handle:expr, $panic_value:expr, $body:block) => {{
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(value) => value,
            Err(payload) => {
                crate::poison_handle($handle, crate::panic_message(payload));
                $panic_value
            }
        }
    }};
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload crossed the ABI boundary".to_owned()
    }
}

fn poison_handle(handle: Option<ZeHandle>, message: String) {
    #[cfg(feature = "text")]
    if handle.is_some_and(text_registry::is_text_handle) {
        if let Some(message) = text_registry::poison(handle, message) {
            registry::poison(None, message);
        }
        return;
    }
    registry::poison(handle, message);
}

fn set_handle_error(handle: Option<ZeHandle>, message: String) {
    #[cfg(feature = "text")]
    if handle.is_some_and(text_registry::is_text_handle) {
        if let Some(message) = text_registry::set_error(handle, message) {
            registry::set_error(None, message);
        }
        return;
    }
    registry::set_error(handle, message);
}

fn handle_error(handle: ZeHandle) -> Result<String, FfiError> {
    #[cfg(feature = "text")]
    if text_registry::is_text_handle(handle) {
        return text_registry::last_error(handle);
    }
    registry::last_error(handle)
}

fn finish(handle: Option<ZeHandle>, result: Result<(), FfiError>) -> ZeErrorCode {
    match result {
        Ok(()) => ZeErrorCode::ZeOk,
        Err(error) => {
            if error.code != ZeErrorCode::ZeErrPoisoned {
                set_handle_error(handle, error.message);
            }
            error.code
        }
    }
}

fn scalar_output<T>(pointer: *mut T) -> Result<(), FfiError> {
    if pointer.is_null() {
        return Err(FfiError::invalid("output pointer is null"));
    }
    if pointer.align_offset(align_of::<T>()) != 0 {
        return Err(FfiError::invalid("output pointer is misaligned"));
    }
    Ok(())
}

fn bool_u32(value: bool) -> u32 {
    u32::from(value)
}

fn usize_u64(value: usize, field: &'static str) -> Result<u64, FfiError> {
    u64::try_from(value).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrInternal,
            format!("{field} does not fit the ABI u64 field"),
        )
    })
}

fn doc_id(value: ZeDocId) -> DocId {
    DocId::new((u128::from(value.high) << 64) | u128::from(value.low))
}

fn ffi_doc_id(value: DocId) -> ZeDocId {
    let value = value.get();
    ZeDocId {
        high: (value >> 64) as u64,
        low: value as u64,
    }
}

fn parse_access(value: i32) -> Result<AccessMode, FfiError> {
    match value {
        0 => Ok(AccessMode::ReadWrite),
        1 => Ok(AccessMode::ReadOnly),
        _ => Err(FfiError::invalid(
            "access_mode discriminant is out of range",
        )),
    }
}

fn parse_durability(value: i32) -> Result<DurabilityMode, FfiError> {
    match value {
        0 => Ok(DurabilityMode::Derived),
        1 => Ok(DurabilityMode::Durable),
        2 => Ok(DurabilityMode::Attached),
        _ => Err(FfiError::invalid(
            "durability_mode discriminant is out of range",
        )),
    }
}

fn parse_commit_tier(value: i32) -> Result<CommitTier, FfiError> {
    match value {
        0 => Ok(CommitTier::None),
        1 => Ok(CommitTier::Ordered),
        2 => Ok(CommitTier::Durable),
        _ => Err(FfiError::invalid(
            "commit_tier discriminant is out of range",
        )),
    }
}

fn parse_open_options(request: &ZeOpenRequest) -> Result<OpenOptions, FfiError> {
    let access = parse_access(request.access_mode)?;
    let durability = parse_durability(request.durability_mode)?;
    let tier = parse_commit_tier(request.commit_tier)?;
    Ok(match access {
        AccessMode::ReadWrite => OpenOptions::new(),
        AccessMode::ReadOnly => OpenOptions::read_only(),
    }
    .with_durability(durability, tier)
    .with_reader_drain_timeout(Duration::from_millis(request.reader_drain_timeout_ms))
    .with_max_resident_bytes(request.max_resident_bytes)
    .with_max_temp_bytes(request.max_temp_bytes))
}

fn parse_graph_profile(value: i32) -> Result<GraphSearchProfile, FfiError> {
    match value {
        0 => Ok(GraphSearchProfile::SiftClass),
        1 => Ok(GraphSearchProfile::Angular),
        _ => Err(FfiError::invalid(
            "graph_profile discriminant is out of range",
        )),
    }
}

fn parse_search_options(request: ZeSearchRequest) -> Result<SearchOptions, FfiError> {
    if request.reserved != 0 {
        return Err(FfiError::invalid("search reserved field must be zero"));
    }
    let mut options = SearchOptions::new(ScanOptions {
        thread_budget: request.thread_budget,
    });
    if let Some(tier) = parse_tier_fields(
        request.has_tier,
        request.tier,
        request.graph_profile,
        request.graph_ef,
        request.graph_seed,
    )? {
        options = options.with_tier(tier);
    }
    Ok(options)
}

fn empty_search_result(abi_size: u32) -> ZeSearchResult {
    ZeSearchResult {
        abi_size,
        abi_reserved: 0,
        hits: std::ptr::null_mut(),
        hit_count: 0,
        generation: 0,
        dims_touched: 0,
        bytes_read: 0,
        threads_used: 0,
        graph_segments_traversed: 0,
        graph_validations: 0,
        graph_entry_seed_discoveries: 0,
        graph_visited_epoch_clears: 0,
        graph_candidates_scored: 0,
        graph_candidates_rescored: 0,
        graph_segments_pruned_by_bound: 0,
    }
}

#[cfg(feature = "abi-panic-probe")]
fn named_panic_probe() -> &'static std::sync::Mutex<Option<&'static str>> {
    static PROBE: std::sync::OnceLock<std::sync::Mutex<Option<&'static str>>> =
        std::sync::OnceLock::new();
    PROBE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Arms a one-shot named panic used only by the ABI wrapper contract tests.
#[cfg(feature = "abi-panic-probe")]
pub fn arm_abi_panic_probe(entry_point: &'static str) {
    if let Ok(mut probe) = named_panic_probe().lock() {
        *probe = Some(entry_point);
    }
}

#[cfg(feature = "abi-panic-probe")]
fn run_named_panic_probe(entry_point: &str) {
    let armed = named_panic_probe()
        .lock()
        .ok()
        .and_then(|mut probe| probe.take());
    if armed == Some(entry_point) {
        std::panic::resume_unwind(Box::new(format!("abi panic probe in {entry_point}")));
    }
}

#[cfg(not(feature = "abi-panic-probe"))]
fn run_named_panic_probe(_entry_point: &str) {}

#[cfg(feature = "abi-panic-probe")]
fn run_panic_probe(request: *const ZeSearchRequest) {
    const PROBE: u32 = 0x5041_4e49;
    if request.is_null() || request.align_offset(align_of::<ZeSearchRequest>()) != 0 {
        return;
    }
    let reserved = unsafe { std::ptr::read(request.cast::<u32>().add(1)) };
    if reserved == PROBE {
        std::panic::resume_unwind(Box::new("abi panic probe"));
    }
}

#[cfg(not(feature = "abi-panic-probe"))]
fn run_panic_probe(_request: *const ZeSearchRequest) {}

fn parse_normalization(value: i32) -> Result<Normalization, FfiError> {
    match value {
        0 => Ok(Normalization::None),
        1 => Ok(Normalization::L2),
        _ => Err(FfiError::invalid(
            "normalization discriminant is out of range",
        )),
    }
}

fn parse_runtime(value: i32) -> Result<EmbeddingRuntime, FfiError> {
    match value {
        1 => Ok(EmbeddingRuntime::CoreMl),
        2 => Ok(EmbeddingRuntime::Mlx),
        3 => Ok(EmbeddingRuntime::CpuReference),
        _ => Err(FfiError::invalid("runtime discriminant is out of range")),
    }
}

fn parse_compute_units(value: i32) -> Result<ComputeUnits, FfiError> {
    match value {
        1 => Ok(ComputeUnits::Cpu),
        2 => Ok(ComputeUnits::CpuAndGpu),
        3 => Ok(ComputeUnits::CpuAndNeuralEngine),
        4 => Ok(ComputeUnits::All),
        _ => Err(FfiError::invalid(
            "compute_units discriminant is out of range",
        )),
    }
}

fn utf8_field(pointer: *const u8, length: usize, field: &'static str) -> Result<String, FfiError> {
    let bytes = marshal::read_slice(pointer, length)?;
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| FfiError::invalid(format!("{field} is not valid UTF-8")))
}

fn parse_tower(tower: ZeEmbeddingTower) -> Result<EmbeddingTower, FfiError> {
    let os_build = match tower.has_os_build {
        0 => {
            if tower.os_build_len != 0 {
                return Err(FfiError::invalid(
                    "os_build bytes were supplied without has_os_build",
                ));
            }
            None
        }
        1 => Some(utf8_field(tower.os_build, tower.os_build_len, "os_build")?),
        _ => return Err(FfiError::invalid("has_os_build must be zero or one")),
    };
    Ok(EmbeddingTower {
        model_id: utf8_field(tower.model_id, tower.model_id_len, "model_id")?,
        model_version: utf8_field(
            tower.model_version,
            tower.model_version_len,
            "model_version",
        )?,
        weights_digest: marshal::copy_slice(tower.weights_digest, tower.weights_digest_len)?,
        dims: tower.dims,
        normalization: parse_normalization(tower.normalization)?,
        prompt_prefix: utf8_field(
            tower.prompt_prefix,
            tower.prompt_prefix_len,
            "prompt_prefix",
        )?,
        max_tokens: tower.max_tokens,
        runtime: parse_runtime(tower.runtime)?,
        compute_units: parse_compute_units(tower.compute_units)?,
        os_build,
    })
}

fn parse_epoch(request: *const ZeEpochRequest) -> Result<StoreEpoch, FfiError> {
    let request = marshal::read_struct(request)?;
    if request.reserved != 0 {
        return Err(FfiError::invalid(
            "epoch request reserved field must be zero",
        ));
    }
    let tokenizer = match request.tokenizer_profile {
        0 => TokenizerConfig::text_default().epoch(),
        _ => {
            return Err(FfiError::invalid(
                "tokenizer_profile discriminant is out of range",
            ));
        }
    };
    Ok(StoreEpoch {
        embedding: EmbeddingEpoch {
            document: parse_tower(request.embedding.document)?,
            query: parse_tower(request.embedding.query)?,
            alignment_digest: marshal::copy_slice(
                request.embedding.alignment_digest,
                request.embedding.alignment_digest_len,
            )?,
        },
        tokenizer,
    })
}

fn canonical_namespace_epoch(
    model_id: &str,
    dimensions: u32,
    normalization: Normalization,
) -> StoreEpoch {
    let tower = EmbeddingTower {
        model_id: model_id.to_owned(),
        model_version: "1".to_owned(),
        weights_digest: Vec::new(),
        dims: dimensions,
        normalization,
        prompt_prefix: String::new(),
        max_tokens: 0,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            document: tower.clone(),
            query: tower,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn record_only_epoch_identity() -> EpochIdentity {
    static IDENTITY: std::sync::OnceLock<EpochIdentity> = std::sync::OnceLock::new();
    *IDENTITY.get_or_init(|| {
        canonical_namespace_epoch("zeppelin.record-only", 1, Normalization::None).identity()
    })
}

fn parse_attribute_type(value: i32) -> Result<ColumnType, FfiError> {
    match value {
        1 => Ok(ColumnType::U64),
        2 => Ok(ColumnType::I64),
        3 => Ok(ColumnType::F64),
        4 => Ok(ColumnType::Bool),
        5 => Ok(ColumnType::DictionaryString),
        6 => Ok(ColumnType::RawString),
        _ => Err(FfiError::invalid(
            "attribute_type discriminant is out of range",
        )),
    }
}

fn parse_namespace_spec(request: *const ZeNamespaceSpec) -> Result<(Schema, StoreEpoch), FfiError> {
    let request = marshal::read_struct(request)?;
    let attributes = marshal::read_slice(request.attributes, request.attribute_count)?;
    let mut columns = Vec::new();
    columns.try_reserve_exact(attributes.len()).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "namespace schema allocation failed",
        )
    })?;
    for attribute in attributes {
        if attribute.attribute_id == 0 {
            return Err(FfiError::invalid("attribute_id zero is reserved for ts"));
        }
        columns.push(ColumnDefinition::new(
            ColumnId::new(attribute.attribute_id),
            utf8_field(attribute.name, attribute.name_len, "attribute name")?,
            parse_attribute_type(attribute.attribute_type)?,
            parse_flag(attribute.nullable, "nullable")?,
        ));
    }
    let schema = Schema::new(columns).map_err(|error| FfiError::invalid(error.to_string()))?;
    let has_vector_space = parse_flag(request.has_vector_space, "has_vector_space")?;
    let normalization = parse_normalization(request.normalization)?;
    if !has_vector_space {
        if request.dimensions > 1 {
            return Err(FfiError::invalid(
                "record-only namespace dimensions must be zero or one",
            ));
        }
        if !request.epoch.is_null() {
            return Err(FfiError::invalid(
                "record-only namespace cannot declare an epoch",
            ));
        }
        return Ok((
            schema,
            canonical_namespace_epoch("zeppelin.record-only", 1, Normalization::None),
        ));
    }
    if request.dimensions == 0 {
        return Err(FfiError::invalid(
            "vector namespace dimensions must be nonzero",
        ));
    }
    let epoch = if request.epoch.is_null() {
        canonical_namespace_epoch("zeppelin.vector-space", request.dimensions, normalization)
    } else {
        parse_epoch(request.epoch)?
    };
    if epoch.embedding.document.dims != request.dimensions
        || epoch.embedding.query.dims != request.dimensions
    {
        return Err(FfiError::invalid(
            "declared epoch dimensions do not match the namespace dimensions",
        ));
    }
    if epoch.embedding.document.normalization != normalization
        || epoch.embedding.query.normalization != normalization
    {
        return Err(FfiError::invalid(
            "declared epoch normalization does not match the namespace normalization",
        ));
    }
    Ok((schema, epoch))
}

fn validate_namespace_name(name: &[u8]) -> Result<&str, FfiError> {
    let valid_first = name.first().is_some_and(u8::is_ascii_alphanumeric);
    let valid_tail = name
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-'));
    if !(valid_first && valid_tail && name.len() <= 255) {
        return Err(FfiError::invalid("namespace name is invalid"));
    }
    std::str::from_utf8(name).map_err(|_| FfiError::invalid("namespace name is not ASCII"))
}

fn parse_attribute_value(
    schema: &Schema,
    attribute: ZeAttributeValue,
) -> Result<Option<(ColumnId, PredicateValue)>, FfiError> {
    if attribute.attribute_id == 0 {
        return Err(FfiError::invalid("attribute_id zero is reserved for ts"));
    }
    let column = ColumnId::new(attribute.attribute_id);
    let value = match attribute.value_type {
        0 => {
            let definition = schema
                .column(column)
                .ok_or_else(|| FfiError::invalid(format!("unknown column {}", column.get())))?;
            if !definition.is_nullable() {
                return Err(FfiError::invalid(format!(
                    "column {} is not nullable",
                    column.get()
                )));
            }
            return Ok(None);
        }
        1 => PredicateValue::U64(attribute.u64_value),
        2 => PredicateValue::I64(attribute.i64_value),
        3 => PredicateValue::F64(attribute.f64_value),
        4 => PredicateValue::Bool(parse_flag(attribute.bool_value, "attribute bool value")?),
        5 => PredicateValue::String(utf8_field(
            attribute.string_value,
            attribute.string_len,
            "attribute string value",
        )?),
        _ => {
            return Err(FfiError::invalid(
                "attribute value_type discriminant is out of range",
            ));
        }
    };
    Ok(Some((column, value)))
}

const MAX_FILTER_NODES: usize = 1 << 20;
const MAX_FILTER_DEPTH: usize = 32;

fn parse_filter_value(
    schema: &Schema,
    column: ColumnId,
    value: ZeAttributeValue,
) -> Result<PredicateValue, FfiError> {
    if value.attribute_id != column.get() {
        return Err(FfiError::invalid(format!(
            "filter value column {} differs from node column {}",
            value.attribute_id,
            column.get()
        )));
    }
    let parsed = match value.value_type {
        1 => PredicateValue::U64(value.u64_value),
        2 => PredicateValue::I64(value.i64_value),
        3 => PredicateValue::F64(value.f64_value),
        4 => PredicateValue::Bool(parse_flag(value.bool_value, "filter bool value")?),
        5 => PredicateValue::String(utf8_field(
            value.string_value,
            value.string_len,
            "filter string value",
        )?),
        0 => return Err(FfiError::invalid("filter values cannot be null")),
        _ => {
            return Err(FfiError::invalid(
                "filter value_type discriminant is out of range",
            ));
        }
    };
    let expected = schema
        .column(column)
        .ok_or_else(|| FfiError::invalid(format!("unknown column {}", column.get())))?
        .column_type();
    let actual = match &parsed {
        PredicateValue::U64(_) => ColumnType::U64,
        PredicateValue::I64(_) => ColumnType::I64,
        PredicateValue::F64(_) => ColumnType::F64,
        PredicateValue::Bool(_) => ColumnType::Bool,
        PredicateValue::String(_) => ColumnType::RawString,
    };
    if expected != actual
        && !matches!(
            (expected, actual),
            (ColumnType::DictionaryString, ColumnType::RawString)
        )
    {
        return Err(FfiError::invalid(format!(
            "column {} expects {expected:?}, received {actual:?}",
            column.get()
        )));
    }
    Ok(parsed)
}

fn require_filter_leaf(node: ZeFilterNode, operation: &str) -> Result<(), FfiError> {
    if node.children_count != 0 {
        return Err(FfiError::invalid(format!(
            "filter {operation} cannot have children"
        )));
    }
    Ok(())
}

fn require_filter_no_bounds(node: ZeFilterNode, operation: &str) -> Result<(), FfiError> {
    if node.has_lower != 0 || node.has_upper != 0 {
        return Err(FfiError::invalid(format!(
            "filter {operation} cannot have range bounds"
        )));
    }
    Ok(())
}

fn parse_filter_values(
    schema: &Schema,
    node: ZeFilterNode,
) -> Result<Vec<PredicateValue>, FfiError> {
    let values = marshal::read_slice(node.values, node.value_count)?;
    let column = ColumnId::new(node.attribute_id);
    let mut parsed = Vec::new();
    parsed.try_reserve_exact(values.len()).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "filter value allocation failed",
        )
    })?;
    for value in values {
        parsed.push(parse_filter_value(schema, column, *value)?);
    }
    Ok(parsed)
}

fn filter_children(
    node: ZeFilterNode,
    node_count: usize,
) -> Result<std::ops::Range<usize>, FfiError> {
    let start = node.children_start as usize;
    let count = node.children_count as usize;
    let end = start
        .checked_add(count)
        .ok_or_else(|| FfiError::invalid("filter child range overflows"))?;
    if start > node_count || end > node_count {
        return Err(FfiError::invalid("filter child index is out of range"));
    }
    Ok(start..end)
}

fn decode_filter_node(
    nodes: &[ZeFilterNode],
    schema: &Schema,
    visited: &mut [bool],
    index: usize,
    depth: usize,
) -> Result<Predicate, FfiError> {
    if depth > MAX_FILTER_DEPTH {
        return Err(FfiError::invalid("filter depth exceeds 32"));
    }
    let node = nodes
        .get(index)
        .copied()
        .ok_or_else(|| FfiError::invalid("filter node index is out of range"))?;
    let visit = visited
        .get_mut(index)
        .ok_or_else(|| FfiError::invalid("filter node index is out of range"))?;
    if *visit {
        return Err(FfiError::invalid(
            "filter node is reachable more than once or forms a cycle",
        ));
    }
    *visit = true;
    let column = ColumnId::new(node.attribute_id);
    match node.op {
        1 | 2 => {
            require_filter_leaf(node, "equality")?;
            require_filter_no_bounds(node, "equality")?;
            if node.value_count != 1 {
                return Err(FfiError::invalid(
                    "filter equality requires exactly one value",
                ));
            }
            let mut values = parse_filter_values(schema, node)?;
            let value = values
                .pop()
                .ok_or_else(|| FfiError::invalid("filter equality value is absent"))?;
            let equality = Predicate::Eq { column, value };
            if node.op == 1 {
                Ok(equality)
            } else {
                Ok(Predicate::Not(Box::new(equality)))
            }
        }
        3 | 4 => {
            require_filter_leaf(node, "membership")?;
            require_filter_no_bounds(node, "membership")?;
            let membership = Predicate::In {
                column,
                values: parse_filter_values(schema, node)?,
            };
            if node.op == 3 {
                Ok(membership)
            } else {
                Ok(Predicate::Not(Box::new(membership)))
            }
        }
        5 => {
            require_filter_leaf(node, "range")?;
            if node.value_count != 0 {
                return Err(FfiError::invalid("filter range cannot have values"));
            }
            let has_lower = parse_flag(node.has_lower, "filter has_lower")?;
            let has_upper = parse_flag(node.has_upper, "filter has_upper")?;
            if !has_lower && !has_upper {
                return Err(FfiError::invalid(
                    "filter range requires at least one bound",
                ));
            }
            let lower = has_lower
                .then(|| {
                    Ok::<RangeBound, FfiError>(RangeBound {
                        value: parse_filter_value(schema, column, node.lower)?,
                        inclusive: parse_flag(node.lower_inclusive, "filter lower_inclusive")?,
                    })
                })
                .transpose()?;
            let upper = has_upper
                .then(|| {
                    Ok::<RangeBound, FfiError>(RangeBound {
                        value: parse_filter_value(schema, column, node.upper)?,
                        inclusive: parse_flag(node.upper_inclusive, "filter upper_inclusive")?,
                    })
                })
                .transpose()?;
            Ok(Predicate::Range(RangePredicate {
                column,
                lower,
                upper,
            }))
        }
        6 | 7 => {
            require_filter_leaf(node, "presence")?;
            require_filter_no_bounds(node, "presence")?;
            if node.value_count != 0 {
                return Err(FfiError::invalid("filter presence cannot have values"));
            }
            if schema.column(column).is_none() {
                return Err(FfiError::invalid(format!(
                    "unknown column {}",
                    column.get()
                )));
            }
            if node.op == 6 {
                Ok(Predicate::Exists(column))
            } else {
                Ok(Predicate::IsNull(column))
            }
        }
        8..=10 => {
            require_filter_no_bounds(node, "logical operator")?;
            if node.value_count != 0 {
                return Err(FfiError::invalid(
                    "filter logical operator cannot have values",
                ));
            }
            if node.op == 10 && node.children_count != 1 {
                return Err(FfiError::invalid("filter not requires exactly one child"));
            }
            let children = filter_children(node, nodes.len())?;
            let mut decoded = Vec::new();
            decoded.try_reserve_exact(children.len()).map_err(|_| {
                FfiError::new(
                    ZeErrorCode::ZeErrOutOfMemory,
                    "filter child allocation failed",
                )
            })?;
            for child in children {
                decoded.push(decode_filter_node(
                    nodes,
                    schema,
                    visited,
                    child,
                    depth.saturating_add(1),
                )?);
            }
            match node.op {
                8 => Ok(Predicate::And(decoded)),
                9 => Ok(Predicate::Or(decoded)),
                10 => decoded
                    .pop()
                    .map(Box::new)
                    .map(Predicate::Not)
                    .ok_or_else(|| FfiError::invalid("filter not child is absent")),
                _ => Err(FfiError::invalid("filter operator is out of range")),
            }
        }
        _ => Err(FfiError::invalid("filter operator is out of range")),
    }
}

fn decode_filter(pointer: *const ZeFilter, schema: &Schema) -> Result<Option<Predicate>, FfiError> {
    if pointer.is_null() {
        return Ok(None);
    }
    let filter = marshal::read_struct(pointer)?;
    if filter.node_count == 0 {
        return Err(FfiError::invalid("filter node_count must be nonzero"));
    }
    if filter.node_count > MAX_FILTER_NODES {
        return Err(FfiError::invalid("filter node_count exceeds the ABI limit"));
    }
    let nodes = marshal::read_slice(filter.nodes, filter.node_count)?;
    let root = filter.root as usize;
    if root >= nodes.len() {
        return Err(FfiError::invalid("filter root is out of range"));
    }
    let mut visited = Vec::new();
    visited.try_reserve_exact(nodes.len()).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "filter visited-set allocation failed",
        )
    })?;
    visited.resize(nodes.len(), false);
    let predicate = decode_filter_node(nodes, schema, &mut visited, root, 1)?;
    zeppelin_embed::planner::validate_predicate(&predicate, schema)
        .map_err(|error| FfiError::invalid(error.to_string()))?;
    Ok(Some(predicate))
}

fn open_store_at(
    path: &Path,
    mut options: OpenOptions,
    epoch: Option<StoreEpoch>,
    schema: Option<Schema>,
    out_handle: *mut ZeHandle,
) -> Result<(), FfiError> {
    let identity = epoch.as_ref().map(StoreEpoch::identity);
    if let Some(epoch) = epoch {
        options = options.with_epoch(epoch);
    }
    if let Some(schema) = schema {
        options = options.with_schema(schema);
    }
    let store = Store::open(path, options).map_err(FfiError::store)?;
    let handle = registry::insert_store(store, identity)?;
    marshal::write_scalar(out_handle, handle);
    Ok(())
}

fn empty_namespace_list_result(abi_size: u32) -> ZeNamespaceListResult {
    ZeNamespaceListResult {
        abi_size,
        abi_reserved: 0,
        entries: std::ptr::null_mut(),
        entry_count: 0,
    }
}

fn namespace_io(path: &Path, source: std::io::Error) -> FfiError {
    FfiError::new(
        ZeErrorCode::ZeErrIo,
        format!("namespace I/O {}: {source}", path.display()),
    )
}

fn namespace_names(root: &Path) -> Result<Vec<Vec<u8>>, FfiError> {
    let directory = std::fs::read_dir(root).map_err(|source| namespace_io(root, source))?;
    let mut names = Vec::new();
    for entry in directory {
        let entry = entry.map_err(|source| namespace_io(root, source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| namespace_io(&entry.path(), source))?;
        if !file_type.is_dir() {
            continue;
        }
        let manifest = entry.path().join("manifest.ze");
        let metadata = match std::fs::metadata(&manifest) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(namespace_io(&manifest, source)),
        };
        if !metadata.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let encoded = file_name.as_encoded_bytes();
        let mut name = Vec::new();
        name.try_reserve_exact(encoded.len()).map_err(|_| {
            FfiError::new(
                ZeErrorCode::ZeErrOutOfMemory,
                "namespace name allocation failed",
            )
        })?;
        name.extend_from_slice(encoded);
        names.try_reserve(1).map_err(|_| {
            FfiError::new(
                ZeErrorCode::ZeErrOutOfMemory,
                "namespace list allocation failed",
            )
        })?;
        names.push(name);
    }
    names.sort_unstable();
    Ok(names)
}

fn publish_namespace_names(names: &[Vec<u8>]) -> Result<(*mut ZeNamespaceEntry, u32), FfiError> {
    if names.is_empty() {
        return Ok((std::ptr::null_mut(), 0));
    }
    let entry_bytes = names
        .len()
        .checked_mul(size_of::<ZeNamespaceEntry>())
        .ok_or_else(|| FfiError::invalid("namespace entry bytes overflow"))?;
    let names_bytes = names.iter().try_fold(0_usize, |total, name| {
        total
            .checked_add(name.len())
            .ok_or_else(|| FfiError::invalid("namespace name bytes overflow"))
    })?;
    let arena_bytes = entry_bytes
        .checked_add(names_bytes)
        .ok_or_else(|| FfiError::invalid("namespace arena bytes overflow"))?;
    let words = arena_bytes.div_ceil(size_of::<u64>());
    let mut arena = Vec::<u64>::new();
    arena.try_reserve_exact(words).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "namespace result arena allocation failed",
        )
    })?;
    arena.resize(words, 0);
    let mut arena = arena.into_boxed_slice();
    let arena_pointer = arena.as_mut_ptr();
    let entries = arena_pointer.cast::<ZeNamespaceEntry>();
    let mut name_pointer = unsafe { arena_pointer.cast::<u8>().add(entry_bytes) };
    for (index, name) in names.iter().enumerate() {
        unsafe {
            std::ptr::copy_nonoverlapping(name.as_ptr(), name_pointer, name.len());
            std::ptr::write(
                entries.add(index),
                ZeNamespaceEntry {
                    name: name_pointer,
                    name_len: name.len(),
                },
            );
            name_pointer = name_pointer.add(name.len());
        }
    }
    let generation = registry::register_result_arena(arena_pointer, arena.len(), names.len())?;
    let _raw = Box::into_raw(arena);
    Ok((entries, generation))
}

fn empty_get_result(abi_size: u32) -> ZeGetResult {
    ZeGetResult {
        abi_size,
        abi_reserved: 0,
        documents: std::ptr::null_mut(),
        document_count: 0,
        missing_count: 0,
        generation: 0,
    }
}

fn empty_scan_result(abi_size: u32) -> ZeScanResult {
    ZeScanResult {
        abi_size,
        abi_reserved: 0,
        documents: std::ptr::null_mut(),
        document_count: 0,
        generation: 0,
        has_more: 0,
        next_segment_id: [0; 16],
        next_row: 0,
        next_phase: 0,
    }
}

fn get_arena_add(total: usize, bytes: usize) -> Result<usize, FfiError> {
    total
        .checked_add(bytes)
        .ok_or_else(|| FfiError::invalid("get result arena size overflows"))
}

fn get_arena_mul(count: usize, width: usize) -> Result<usize, FfiError> {
    count
        .checked_mul(width)
        .ok_or_else(|| FfiError::invalid("get result arena size overflows"))
}

fn publish_get_documents(
    ids: &[ZeDocId],
    documents: &[Option<StoredDocument>],
) -> Result<(*mut ZeStoredDocument, u32, usize), FfiError> {
    if ids.len() != documents.len() || ids.is_empty() {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrInternal,
            "get result cardinality does not match the nonempty request",
        ));
    }
    let document_bytes = get_arena_mul(documents.len(), size_of::<ZeStoredDocument>())?;
    let attribute_count = documents.iter().try_fold(0_usize, |total, document| {
        get_arena_add(
            total,
            document
                .as_ref()
                .and_then(|document| document.attributes.as_ref())
                .map_or(0, Vec::len),
        )
    })?;
    let attribute_bytes = get_arena_mul(attribute_count, size_of::<ZeAttributeValue>())?;
    let vector_count = documents.iter().try_fold(0_usize, |total, document| {
        get_arena_add(
            total,
            document
                .as_ref()
                .and_then(|document| document.vector.as_ref())
                .map_or(0, Vec::len),
        )
    })?;
    let vector_bytes = get_arena_mul(vector_count, size_of::<f32>())?;
    let payload_bytes =
        documents.iter().try_fold(0_usize, |total, document| {
            let Some(document) = document else {
                return Ok(total);
            };
            let total = get_arena_add(total, document.text.as_ref().map_or(0, String::len))?;
            let total = get_arena_add(total, document.metadata.as_ref().map_or(0, Vec::len))?;
            document.attributes.as_ref().into_iter().flatten().try_fold(
                total,
                |total, (_, value)| match value {
                    PredicateValue::String(value) => get_arena_add(total, value.len()),
                    PredicateValue::U64(_)
                    | PredicateValue::I64(_)
                    | PredicateValue::F64(_)
                    | PredicateValue::Bool(_) => Ok(total),
                },
            )
        })?;
    let vector_offset = get_arena_add(document_bytes, attribute_bytes)?;
    let payload_offset = get_arena_add(vector_offset, vector_bytes)?;
    let arena_bytes = get_arena_add(payload_offset, payload_bytes)?;
    let words = arena_bytes.div_ceil(size_of::<u64>());
    let mut arena = Vec::<u64>::new();
    arena.try_reserve_exact(words).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "get result arena allocation failed",
        )
    })?;
    arena.resize(words, 0);
    let mut arena = arena.into_boxed_slice();
    let arena_pointer = arena.as_mut_ptr();
    let output_documents = arena_pointer.cast::<ZeStoredDocument>();
    let mut attribute_pointer =
        unsafe { arena_pointer.cast::<u8>().add(document_bytes) }.cast::<ZeAttributeValue>();
    let mut vector_pointer = unsafe { arena_pointer.cast::<u8>().add(vector_offset) }.cast::<f32>();
    let mut payload_pointer = unsafe { arena_pointer.cast::<u8>().add(payload_offset) };
    let mut missing_count = 0_usize;
    for (index, (requested_id, document)) in ids.iter().zip(documents).enumerate() {
        let mut output = ZeStoredDocument {
            has_document: 0,
            doc_id: *requested_id,
            revision: 0,
            timestamp: 0,
            vector: std::ptr::null(),
            vector_len: 0,
            text: std::ptr::null(),
            text_len: 0,
            metadata: std::ptr::null(),
            metadata_len: 0,
            attributes: std::ptr::null(),
            attribute_count: 0,
        };
        if let Some(document) = document {
            output.has_document = 1;
            output.doc_id = ffi_doc_id(document.doc_id);
            output.revision = document.revision.get();
            output.timestamp = document.timestamp;
            if let Some(vector) = document.vector.as_ref().filter(|vector| !vector.is_empty()) {
                output.vector = vector_pointer;
                output.vector_len = vector.len();
                unsafe {
                    std::ptr::copy_nonoverlapping(vector.as_ptr(), vector_pointer, vector.len());
                    vector_pointer = vector_pointer.add(vector.len());
                }
            }
            if let Some(text) = document.text.as_ref().filter(|text| !text.is_empty()) {
                output.text = payload_pointer;
                output.text_len = text.len();
                unsafe {
                    std::ptr::copy_nonoverlapping(text.as_ptr(), payload_pointer, text.len());
                    payload_pointer = payload_pointer.add(text.len());
                }
            }
            if let Some(metadata) = document
                .metadata
                .as_ref()
                .filter(|metadata| !metadata.is_empty())
            {
                output.metadata = payload_pointer;
                output.metadata_len = metadata.len();
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        metadata.as_ptr(),
                        payload_pointer,
                        metadata.len(),
                    );
                    payload_pointer = payload_pointer.add(metadata.len());
                }
            }
            if let Some(attributes) = document
                .attributes
                .as_ref()
                .filter(|attributes| !attributes.is_empty())
            {
                output.attributes = attribute_pointer;
                output.attribute_count = attributes.len();
                for (column, value) in attributes {
                    let mut attribute = ZeAttributeValue {
                        attribute_id: column.get(),
                        value_type: 0,
                        u64_value: 0,
                        i64_value: 0,
                        f64_value: 0.0,
                        bool_value: 0,
                        string_value: std::ptr::null(),
                        string_len: 0,
                    };
                    match value {
                        PredicateValue::U64(value) => {
                            attribute.value_type = 1;
                            attribute.u64_value = *value;
                        }
                        PredicateValue::I64(value) => {
                            attribute.value_type = 2;
                            attribute.i64_value = *value;
                        }
                        PredicateValue::F64(value) => {
                            attribute.value_type = 3;
                            attribute.f64_value = *value;
                        }
                        PredicateValue::Bool(value) => {
                            attribute.value_type = 4;
                            attribute.bool_value = bool_u32(*value);
                        }
                        PredicateValue::String(value) => {
                            attribute.value_type = 5;
                            attribute.string_len = value.len();
                            if !value.is_empty() {
                                attribute.string_value = payload_pointer;
                                unsafe {
                                    std::ptr::copy_nonoverlapping(
                                        value.as_ptr(),
                                        payload_pointer,
                                        value.len(),
                                    );
                                    payload_pointer = payload_pointer.add(value.len());
                                }
                            }
                        }
                    }
                    unsafe {
                        std::ptr::write(attribute_pointer, attribute);
                        attribute_pointer = attribute_pointer.add(1);
                    }
                }
            }
        } else {
            missing_count = missing_count.checked_add(1).ok_or_else(|| {
                FfiError::new(ZeErrorCode::ZeErrInternal, "get missing count overflows")
            })?;
        }
        unsafe { std::ptr::write(output_documents.add(index), output) };
    }
    let allocation_generation =
        registry::register_result_arena(arena_pointer, arena.len(), documents.len())?;
    let _raw = Box::into_raw(arena);
    Ok((output_documents, allocation_generation, missing_count))
}

fn open_store(
    request: *const ZeOpenRequest,
    epoch: Option<StoreEpoch>,
    out_handle: *mut ZeHandle,
) -> Result<(), FfiError> {
    let request = marshal::read_struct(request)?;
    scalar_output(out_handle)?;
    let path = marshal::utf8_without_nul(request.path, request.path_len)?;
    if path.is_empty() {
        return Err(FfiError::invalid("store path must not be empty"));
    }
    open_store_at(
        Path::new(path),
        parse_open_options(&request)?,
        epoch,
        None,
        out_handle,
    )
}

#[cfg(feature = "text")]
fn text_open_options(
    request: ZeOpenRequest,
) -> Result<(String, zeppelin_embed_text::TextOpenOptions), FfiError> {
    let request = marshal::read_struct(&request)?;
    let path = marshal::utf8_without_nul(request.path, request.path_len)?.to_owned();
    if path.is_empty() {
        return Err(FfiError::invalid("store path must not be empty"));
    }
    let access = parse_access(request.access_mode)?;
    let durability = parse_durability(request.durability_mode)?;
    let tier = parse_commit_tier(request.commit_tier)?;
    let options = match access {
        AccessMode::ReadWrite => OpenOptions::new(),
        AccessMode::ReadOnly => OpenOptions::read_only(),
    }
    .with_durability(durability, tier)
    .with_reader_drain_timeout(Duration::from_millis(request.reader_drain_timeout_ms))
    .with_max_resident_bytes(request.max_resident_bytes)
    .with_max_temp_bytes(request.max_temp_bytes);
    Ok((
        path,
        zeppelin_embed_text::TextOpenOptions::new(options, TokenizerConfig::text_default()),
    ))
}

fn parse_tier(request: &ZeQueryRequest) -> Result<Option<SearchTier>, FfiError> {
    parse_tier_fields(
        request.has_tier,
        request.tier,
        request.graph_profile,
        request.graph_ef,
        request.graph_seed,
    )
}

fn parse_tier_fields(
    has_tier: u32,
    tier: i32,
    graph_profile: i32,
    graph_ef: usize,
    graph_seed: u64,
) -> Result<Option<SearchTier>, FfiError> {
    let profile = parse_graph_profile(graph_profile)?;
    match (has_tier, tier) {
        (0, 0) => Ok(None),
        (0, _) => Err(FfiError::invalid(
            "tier must be zero when has_tier expresses no preference",
        )),
        (1, 0) => Ok(Some(SearchTier::Auto)),
        (1, 1) => Ok(Some(SearchTier::Exact)),
        (1, 2) => Ok(Some(SearchTier::Scan)),
        (1, 3) => {
            let graph = GraphSearchOptions::new(profile).with_seed(graph_seed);
            let graph = if graph_ef == 0 {
                graph
            } else {
                graph.with_ef(graph_ef)
            };
            Ok(Some(SearchTier::Graph(graph)))
        }
        (1, _) => Err(FfiError::invalid("tier discriminant is out of range")),
        _ => Err(FfiError::invalid("has_tier must be zero or one")),
    }
}

fn parse_flag(value: u32, field: &'static str) -> Result<bool, FfiError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(FfiError::invalid(format!("{field} must be zero or one"))),
    }
}

fn parse_hybrid(request: &ZeQueryRequest, hybrid: bool) -> Result<Option<HybridQuery>, FfiError> {
    let has_alpha = parse_flag(request.has_alpha, "has_alpha")?;
    let rules_enabled = parse_flag(request.rules_enabled, "rules_enabled")?;
    let has_max_rounds = parse_flag(request.has_max_rounds, "has_max_rounds")?;
    let quoted_phrase = parse_flag(request.quoted_phrase, "quoted_phrase")?;
    let identifier_token = parse_flag(request.identifier_token, "identifier_token")?;
    let has_rarest = parse_flag(
        request.has_rarest_exact_document_frequency,
        "has_rarest_exact_document_frequency",
    )?;
    if !hybrid {
        if has_alpha
            || rules_enabled
            || has_max_rounds
            || quoted_phrase
            || identifier_token
            || has_rarest
        {
            return Err(FfiError::invalid(
                "fusion parameters require both a vector and a lexical leg",
            ));
        }
        return Ok(None);
    }
    let mut query = HybridQuery::new(request.k).with_rule_signals(RuleSignals {
        quoted_phrase,
        rarest_exact_document_frequency: has_rarest
            .then_some(request.rarest_exact_document_frequency),
        identifier_token,
    });
    if has_alpha {
        query = query.with_alpha(request.alpha);
    }
    if rules_enabled {
        query = query.with_rules();
    }
    if has_max_rounds {
        let max_rounds = usize::try_from(request.max_rounds)
            .map_err(|_| FfiError::invalid("max_rounds does not fit usize"))?;
        query = query.with_max_rounds(max_rounds);
    }
    Ok(Some(query))
}

fn query_control_for(
    cancel_token: ZeCancelToken,
    deadline_ns: u64,
) -> Result<QueryControl, FfiError> {
    if cancel_token != 0 && deadline_ns != 0 {
        return Err(FfiError::invalid(
            "search accepts either a cancel token or a deadline, not both",
        ));
    }
    if cancel_token != 0 {
        return registry::lookup_cancel(cancel_token).map(QueryControl::Cancel);
    }
    if deadline_ns != 0 {
        return Deadline::after(Duration::from_nanos(deadline_ns))
            .map(QueryControl::Deadline)
            .map_err(|error| FfiError::invalid(error.to_string()));
    }
    Ok(QueryControl::Cancel(CancelToken::new()))
}

fn analyze_query_text(pointer: *const u8, length: usize) -> Result<TermQuery, FfiError> {
    let bytes = marshal::read_slice(pointer, length)?;
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).map_err(FfiError::tokenizer)?;
    let terms = analyzer
        .analyze_bytes(bytes)
        .map_err(FfiError::tokenizer)?
        .into_iter()
        .map(|token| token.term.into_bytes())
        .collect();
    Ok(TermQuery::flat(terms, &[DEFAULT_FIELD]))
}

fn empty_query_result(abi_size: u32) -> ZeQueryResult {
    ZeQueryResult {
        abi_size,
        abi_reserved: 0,
        hits: std::ptr::null_mut(),
        hit_count: 0,
        generation: 0,
        mode: 0,
        approximate: 0,
        exact_rescore: 0,
        budget_exhausted: 0,
        has_fusion: 0,
        fusion_method: 0,
        effective_alpha: 0.0,
        fusion_rounds: 0,
        has_embedding_epoch: 0,
        has_tokenizer_epoch: 0,
        embedding_epoch: 0,
        tokenizer_epoch: 0,
        dims_touched: 0,
        bytes_read: 0,
        docs_evaluated: 0,
        postings_decoded: 0,
    }
}

fn publish_hits<T: Copy + 'static>(hits: Vec<T>) -> Result<(*mut T, usize, u32), FfiError> {
    let mut hits = hits.into_boxed_slice();
    let hit_count = hits.len();
    let hit_pointer = if hits.is_empty() {
        std::ptr::null_mut()
    } else {
        hits.as_mut_ptr()
    };
    let allocation_generation = registry::register_result(hit_pointer, hit_count)?;
    if !hits.is_empty() {
        let _raw = Box::into_raw(hits);
    }
    Ok((hit_pointer, hit_count, allocation_generation))
}

fn fill_diagnostics(
    result: &mut ZeQueryResult,
    diagnostics: &zeppelin_embed::diag::QueryDiagnostics,
) -> Result<(), FfiError> {
    result.generation = diagnostics.snapshot_generation;
    result.approximate = bool_u32(diagnostics.approximate);
    result.exact_rescore = bool_u32(diagnostics.exact_rescore);
    result.budget_exhausted = bool_u32(diagnostics.budget_exhausted);
    if let Some(fusion) = &diagnostics.fusion {
        result.has_fusion = 1;
        result.fusion_method = match fusion.method {
            FusionMethod::ConvexCombination => 0,
            FusionMethod::ReciprocalRankFusion => 1,
        };
        result.effective_alpha = fusion.effective_alpha;
        result.fusion_rounds = usize_u64(fusion.rounds, "fusion_rounds")?;
    }
    if let Some(epoch) = diagnostics.embedding_epoch {
        result.has_embedding_epoch = 1;
        result.embedding_epoch = epoch.value();
    }
    if let Some(epoch) = diagnostics.tokenizer_epoch {
        result.has_tokenizer_epoch = 1;
        result.tokenizer_epoch = epoch.value();
    }
    result.dims_touched = diagnostics.counters.scan.dims_touched;
    result.bytes_read = diagnostics.counters.scan.bytes_read;
    result.docs_evaluated = diagnostics.counters.lexical.docs_evaluated;
    result.postings_decoded = diagnostics.counters.lexical.postings_decoded;
    Ok(())
}

fn ffi_identity(identity: EpochIdentity) -> (u64, u64) {
    (identity.embedding.value(), identity.tokenizer.value())
}

/// Returns the frozen ABI version, `ZE_ABI_VERSION`.
#[unsafe(no_mangle)]
pub extern "C" fn ze_abi_version() -> u32 {
    ffi_entry!(None, 0, { ZE_ABI_VERSION })
}

/// Opens a store and writes a new generation-tagged handle.
///
/// `path` is caller-owned UTF-8 bytes and need only outlive this call.
/// Interior NUL bytes are rejected. `out_handle` is caller-owned.
#[unsafe(no_mangle)]
pub extern "C" fn ze_open(request: *const ZeOpenRequest, out_handle: *mut ZeHandle) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(None, open_store(request, None, out_handle))
    })
}

/// Opens a store while declaring its embedding and tokenizer interpretation.
/// `request` and `epoch` are caller-owned and need only outlive this call;
/// every string and digest inside `epoch` is copied. A store that already
/// carries a different identity returns `ZE_ERR_EPOCH_MISMATCH`. The declared
/// identity is attached to every `ze_ingest` batch made through the handle,
/// which a stamped store requires.
#[unsafe(no_mangle)]
pub extern "C" fn ze_open_with_epoch(
    request: *const ZeOpenRequest,
    epoch: *const ZeEpochRequest,
    out_handle: *mut ZeHandle,
) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(
            None,
            parse_epoch(epoch).and_then(|epoch| open_store(request, Some(epoch), out_handle)),
        )
    })
}

/// Opens or idempotently creates `root/name` with the declared namespace spec.
/// All request data is caller-owned and need only outlive this call. The
/// returned handle is an ordinary store handle accepted by every existing
/// store function.
#[unsafe(no_mangle)]
pub extern "C" fn ze_namespace_open(
    request: *const ZeNamespaceOpenRequest,
    out_handle: *mut ZeHandle,
) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_namespace_open");
        finish(
            None,
            (|| {
                let request = marshal::read_struct(request)?;
                scalar_output(out_handle)?;
                let root = marshal::utf8_without_nul(request.root, request.root_len)?;
                if root.is_empty() {
                    return Err(FfiError::invalid("namespace root must not be empty"));
                }
                let name_bytes = marshal::read_slice(request.name, request.name_len)?;
                let name = validate_namespace_name(name_bytes)?;
                let open = marshal::read_struct(&request.open)?;
                let (schema, epoch) = parse_namespace_spec(request.spec)?;
                let path = Path::new(root).join(name);
                open_store_at(
                    &path,
                    parse_open_options(&open)?,
                    Some(epoch),
                    Some(schema),
                    out_handle,
                )
            })(),
        )
    })
}

/// Lists direct child directories of `root` that contain a `manifest.ze`, in
/// ascending byte order. The returned names share one callee-owned arena.
#[unsafe(no_mangle)]
pub extern "C" fn ze_namespace_list(
    request: *const ZeNamespaceListRequest,
    out_result: *mut ZeNamespaceListResult,
) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_namespace_list");
        finish(
            None,
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_result)?;
                let root = marshal::utf8_without_nul(request.root, request.root_len)?;
                if root.is_empty() {
                    return Err(FfiError::invalid("namespace root must not be empty"));
                }
                let names = namespace_names(Path::new(root))?;
                let (entries, allocation_generation) = publish_namespace_names(&names)?;
                marshal::write_output(
                    out_result,
                    ZeNamespaceListResult {
                        abi_size,
                        abi_reserved: allocation_generation,
                        entries,
                        entry_count: names.len(),
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Releases the single arena owned by a namespace-list result. A zeroed result
/// is accepted as a successful no-op.
#[unsafe(no_mangle)]
pub extern "C" fn ze_namespace_list_result_free(result: *mut ZeNamespaceListResult) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_namespace_list_result_free");
        finish(
            None,
            (|| {
                if result.is_null() {
                    return Err(FfiError::invalid("namespace list result pointer is null"));
                }
                if result.align_offset(align_of::<ZeNamespaceListResult>()) != 0 {
                    return Err(FfiError::invalid(
                        "namespace list result pointer is misaligned",
                    ));
                }
                let abi_size = marshal::read_abi_size(result);
                if abi_size == 0 {
                    let zeroed = marshal::read_value(result);
                    if zeroed.entries.is_null() && zeroed.entry_count == 0 {
                        return Ok(());
                    }
                    return Err(FfiError::invalid(
                        "zero-sized namespace list result contains an allocation",
                    ));
                }
                marshal::validate_abi_size::<ZeNamespaceListResult>(abi_size)?;
                let current = marshal::read_value(result);
                if current.entries.is_null() != (current.entry_count == 0) {
                    return Err(FfiError::invalid(
                        "namespace result pointer and entry count disagree",
                    ));
                }
                registry::take_registered_result(
                    current.entries.cast::<u64>(),
                    current.entry_count,
                    current.abi_reserved,
                )?;
                marshal::write_output(result, empty_namespace_list_result(abi_size));
                Ok(())
            })(),
        )
    })
}

/// Computes the compact identity of a caller-declared epoch without touching
/// any store. `epoch` is caller-owned; `out_identity` is caller-owned and
/// must have `abi_size` initialized.
#[unsafe(no_mangle)]
pub extern "C" fn ze_epoch_identity(
    epoch: *const ZeEpochRequest,
    out_identity: *mut ZeEpochIdentity,
) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(
            None,
            (|| {
                let abi_size = marshal::validate_output(out_identity)?;
                let (embedding_epoch, tokenizer_epoch) =
                    ffi_identity(parse_epoch(epoch)?.identity());
                marshal::write_output(
                    out_identity,
                    ZeEpochIdentity {
                        abi_size,
                        abi_reserved: 0,
                        embedding_epoch,
                        tokenizer_epoch,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Reads the identity currently published by an open store. A store that
/// carries no stamped epoch returns `ZE_ERR_EPOCH_UNSTAMPED`. `out_identity`
/// is caller-owned and must have `abi_size` initialized.
#[unsafe(no_mangle)]
pub extern "C" fn ze_epoch_current(
    handle: ZeHandle,
    out_identity: *mut ZeEpochIdentity,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_epoch_current");
        finish(
            Some(handle),
            (|| {
                let abi_size = marshal::validate_output(out_identity)?;
                let access = registry::lookup(handle)?;
                let identity = access.store.epoch_identity().ok_or_else(|| {
                    FfiError::new(
                        ZeErrorCode::ZeErrEpochUnstamped,
                        "store carries no stamped epoch identity",
                    )
                })?;
                let (embedding_epoch, tokenizer_epoch) = ffi_identity(identity);
                marshal::write_output(
                    out_identity,
                    ZeEpochIdentity {
                        abi_size,
                        abi_reserved: 0,
                        embedding_epoch,
                        tokenizer_epoch,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Atomically publishes a registered epoch whose segments are retained.
/// Requires a sealed active segment; a writer-slot conflict returns
/// `ZE_ERR_BUSY`. Not cancellable in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_epoch_switch_alias(
    handle: ZeHandle,
    target: *const ZeEpochRequest,
    out_report: *mut ZeEpochAliasReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_epoch_switch_alias");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let target = parse_epoch(target)?.identity();
                let abi_size = marshal::validate_output(out_report)?;
                let report = access
                    .store
                    .switch_epoch_alias(target)
                    .map_err(FfiError::epoch_transition)?;
                let (previous_embedding_epoch, previous_tokenizer_epoch) =
                    ffi_identity(report.previous());
                let (published_embedding_epoch, published_tokenizer_epoch) =
                    ffi_identity(report.published());
                marshal::write_output(
                    out_report,
                    ZeEpochAliasReport {
                        abi_size,
                        abi_reserved: 0,
                        generation: report.generation(),
                        previous_embedding_epoch,
                        previous_tokenizer_epoch,
                        published_embedding_epoch,
                        published_tokenizer_epoch,
                        manifest_committed: bool_u32(report.manifest_committed()),
                        reserved: 0,
                    },
                );
                Ok(())
            }),
        )
    })
}

/// Explicitly drops every immutable segment belonging to the embedding epoch
/// named by `target.embedding`; the tokenizer profile is validated but not
/// used. Not cancellable in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_epoch_drop(
    handle: ZeHandle,
    target: *const ZeEpochRequest,
    out_report: *mut ZeEpochDropReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_epoch_drop");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let target = parse_epoch(target)?.identity().embedding;
                let abi_size = marshal::validate_output(out_report)?;
                let report = access
                    .store
                    .drop_epoch(target)
                    .map_err(FfiError::epoch_transition)?;
                marshal::write_output(
                    out_report,
                    ZeEpochDropReport {
                        abi_size,
                        abi_reserved: 0,
                        generation: report.generation(),
                        segments_dropped: usize_u64(
                            report.segments_dropped().len(),
                            "segments_dropped",
                        )?,
                        bytes_reclaimed: report.bytes_reclaimed(),
                    },
                );
                Ok(())
            }),
        )
    })
}

/// Opens a text store bound to one immutable `.zem` model bundle.
#[cfg(feature = "text")]
#[unsafe(no_mangle)]
pub extern "C" fn ze_text_open(
    request: *const ZeTextOpenRequest,
    out_handle: *mut ZeHandle,
) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(
            None,
            (|| {
                let request = marshal::read_struct(request)?;
                scalar_output(out_handle)?;
                let (store_path, options) = text_open_options(request.store)?;
                let bundle_path =
                    marshal::utf8_without_nul(request.bundle_path, request.bundle_path_len)?;
                if bundle_path.is_empty() {
                    return Err(FfiError::invalid("bundle path must not be empty"));
                }
                let store = zeppelin_embed_text::TextStore::open(
                    Path::new(&store_path),
                    Path::new(bundle_path),
                    options,
                )
                .map_err(FfiError::text)?;
                let handle = text_registry::insert(store)?;
                marshal::write_scalar(out_handle, handle);
                Ok(())
            })(),
        )
    })
}

/// Ingests text through the bounded tokenizer, MLX, writer, and maintenance pipeline.
#[cfg(feature = "text")]
#[unsafe(no_mangle)]
pub extern "C" fn ze_text_ingest(
    handle: ZeHandle,
    request: *const ZeTextIngestRequest,
    out_report: *mut ZeMutationReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_text_ingest");
        finish(
            Some(handle),
            text_registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let records = marshal::read_slice(request.documents, request.document_count)?;
                if records.is_empty() {
                    return Err(FfiError::new(
                        ZeErrorCode::ZeErrEmptyBatch,
                        "text ingest batch is empty",
                    ));
                }
                let mut documents = Vec::new();
                documents.try_reserve_exact(records.len()).map_err(|_| {
                    FfiError::new(
                        ZeErrorCode::ZeErrOutOfMemory,
                        "text document allocation failed",
                    )
                })?;
                for record in records {
                    let record = marshal::read_struct(record as *const ZeTextDocument)?;
                    documents.push(zeppelin_embed_text::TextDocument::new(
                        doc_id(record.doc_id).get(),
                        record.revision,
                        utf8_field(record.text, record.text_len, "text document")?,
                    ));
                }
                let defaults = zeppelin_embed_text::IngestOptions::default();
                let report = access
                    .store
                    .ingest_text(
                        &documents,
                        zeppelin_embed_text::IngestOptions {
                            embed_batch_size: request.embed_batch_size,
                            seal_every: request.seal_every,
                            channel_capacity: request.channel_capacity,
                            ..defaults
                        },
                    )
                    .map_err(FfiError::text)?;
                marshal::write_output(
                    out_report,
                    ZeMutationReport {
                        abi_size,
                        abi_reserved: 0,
                        sequence: 0,
                        generation: report.generation,
                    },
                );
                Ok(())
            }),
        )
    })
}

/// Repeats bounded text-store maintenance slices until all due work completes.
#[cfg(feature = "text")]
#[unsafe(no_mangle)]
pub extern "C" fn ze_text_maintain(
    handle: ZeHandle,
    request: *const ZeMaintainRequest,
    out_report: *mut ZeMaintainReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_text_maintain");
        finish(
            Some(handle),
            text_registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let report = access
                    .store
                    .maintain_to_completion(MaintenanceBudget {
                        wall_time: Duration::from_nanos(request.wall_time_ns),
                        bytes: request.bytes,
                    })
                    .map_err(FfiError::text)?;
                marshal::write_output(
                    out_report,
                    ZeMaintainReport {
                        abi_size,
                        abi_reserved: 0,
                        graphs_built: report.graphs_built,
                        bytes_consumed: report.bytes_consumed,
                        checkpoints_resumed: report.checkpoints_resumed,
                        status: 0,
                        reserved: 0,
                    },
                );
                Ok(())
            }),
        )
    })
}

/// Executes one dense, lexical, or hybrid text query and returns stored text on every hit.
#[cfg(feature = "text")]
#[unsafe(no_mangle)]
pub extern "C" fn ze_text_query(
    handle: ZeHandle,
    request: *const ZeTextQueryRequest,
    out_result: *mut ZeTextQueryResult,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_text_query");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_result)?;
                if request.reserved != 0 {
                    return Err(FfiError::invalid("text query reserved field must be zero"));
                }
                let legs = match request.legs {
                    0 => zeppelin_embed_text::Legs::Dense,
                    1 => zeppelin_embed_text::Legs::Lexical,
                    2 => zeppelin_embed_text::Legs::Hybrid,
                    _ => return Err(FfiError::invalid("text query legs is out of range")),
                };
                let text = utf8_field(request.text, request.text_len, "query text")?;
                let access = text_registry::lookup(handle)?;
                let source = access
                    .store
                    .query_text(
                        &text,
                        zeppelin_embed_text::QueryOptions::new(request.k).with_legs(legs),
                    )
                    .map_err(FfiError::text)?;
                let epoch = access.store.epoch();
                let mut hits = Vec::new();
                let mut texts = Vec::new();
                hits.try_reserve_exact(source.len()).map_err(|_| {
                    FfiError::new(ZeErrorCode::ZeErrOutOfMemory, "text hit allocation failed")
                })?;
                texts.try_reserve_exact(source.len()).map_err(|_| {
                    FfiError::new(ZeErrorCode::ZeErrOutOfMemory, "text allocation failed")
                })?;
                for hit in source {
                    let mut text = hit.text.into_bytes().into_boxed_slice();
                    let text_len = text.len();
                    let text_pointer = if text.is_empty() {
                        std::ptr::null_mut()
                    } else {
                        text.as_mut_ptr()
                    };
                    hits.push(ZeTextQueryHit {
                        doc_id: ffi_doc_id(DocId::new(hit.doc_id)),
                        revision: hit.revision,
                        chunk: hit.chunk,
                        reserved: 0,
                        text: text_pointer,
                        text_len,
                        score: hit.score,
                        has_vector_score: bool_u32(hit.vector_squared_l2.is_some()),
                        has_lexical_score: bool_u32(hit.lexical_bm25.is_some()),
                        vector_squared_l2: hit.vector_squared_l2.unwrap_or(0.0),
                        lexical_bm25: hit.lexical_bm25.unwrap_or(0.0),
                    });
                    texts.push(text);
                }
                let (hit_pointer, hit_count, allocation_generation) = publish_hits(hits)?;
                for text in texts {
                    if !text.is_empty() {
                        let _raw = Box::into_raw(text);
                    }
                }
                marshal::write_output(
                    out_result,
                    ZeTextQueryResult {
                        abi_size,
                        abi_reserved: allocation_generation,
                        hits: hit_pointer,
                        hit_count,
                        embedding_epoch: epoch.embedding.value(),
                        tokenizer_epoch: epoch.tokenizer.value(),
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Frees a text query result and every callee-owned hit string exactly once.
#[cfg(feature = "text")]
#[unsafe(no_mangle)]
pub extern "C" fn ze_text_query_result_free(result: *mut ZeTextQueryResult) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(
            None,
            (|| {
                if result.is_null() {
                    return Err(FfiError::invalid("text result pointer is null"));
                }
                if result.align_offset(align_of::<ZeTextQueryResult>()) != 0 {
                    return Err(FfiError::invalid("text result pointer is misaligned"));
                }
                let abi_size = marshal::read_abi_size(result);
                if abi_size == 0 {
                    let zeroed = marshal::read_value(result);
                    if zeroed.hits.is_null() && zeroed.hit_count == 0 {
                        return Ok(());
                    }
                    return Err(FfiError::invalid(
                        "zero-sized text result contains an allocation",
                    ));
                }
                marshal::validate_abi_size::<ZeTextQueryResult>(abi_size)?;
                let current = marshal::read_value(result);
                if current.hit_count == 0 {
                    registry::take_result(current.hits, current.hit_count, current.abi_reserved)?;
                } else {
                    let hits = registry::take_result_box(
                        current.hits,
                        current.hit_count,
                        current.abi_reserved,
                    )?;
                    for hit in &hits {
                        if hit.text.is_null() != (hit.text_len == 0) {
                            return Err(FfiError::invalid("text hit pointer and length disagree"));
                        }
                        if !hit.text.is_null() {
                            let text = std::ptr::slice_from_raw_parts_mut(hit.text, hit.text_len);
                            unsafe { drop(Box::from_raw(text)) };
                        }
                    }
                }
                marshal::write_output(
                    result,
                    ZeTextQueryResult {
                        abi_size,
                        abi_reserved: 0,
                        hits: std::ptr::null_mut(),
                        hit_count: 0,
                        embedding_epoch: 0,
                        tokenizer_epoch: 0,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Releases the slot and store. A poisoned handle is still released and
/// returns `ZE_ERR_POISONED`. All other calls reject poison before touching
/// the store.
#[unsafe(no_mangle)]
pub extern "C" fn ze_close(handle: ZeHandle) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_close");
        #[cfg(feature = "text")]
        if text_registry::is_text_handle(handle) {
            return finish(Some(handle), text_registry::close(handle));
        }
        match registry::begin_close(handle) {
            Ok(registry::CloseAccess::Poisoned) => ZeErrorCode::ZeErrPoisoned,
            Ok(registry::CloseAccess::Store(store)) => {
                let close_result = store.close().map_err(FfiError::store);
                let release_result = registry::finish_close(handle);
                finish(Some(handle), close_result.and(release_result))
            }
            Err(error) => finish(Some(handle), Err(error)),
        }
    })
}

/// Reads the explicit store lifecycle state. `out_report` is caller-owned
/// and must have `abi_size` initialized.
#[unsafe(no_mangle)]
pub extern "C" fn ze_state(handle: ZeHandle, out_report: *mut ZeStateReport) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_state");
        finish(
            Some(handle),
            (|| {
                let abi_size = marshal::validate_output(out_report)?;
                let access = registry::lookup(handle)?;
                let state = match access.store.state().map_err(FfiError::store)? {
                    StoreState::Open => 0,
                    StoreState::Closing => 1,
                    StoreState::Closed => 2,
                };
                marshal::write_output(
                    out_report,
                    ZeStateReport {
                        abi_size,
                        abi_reserved: 0,
                        state,
                        reserved: 0,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Reads exact resource counters for an open store. `out_report` is
/// caller-owned and must have `abi_size` initialized.
#[unsafe(no_mangle)]
pub extern "C" fn ze_stats(handle: ZeHandle, out_report: *mut ZeStatsReport) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_stats");
        finish(
            Some(handle),
            (|| {
                let abi_size = marshal::validate_output(out_report)?;
                let access = registry::lookup(handle)?;
                let stats = access.store.stats().map_err(FfiError::store)?;
                marshal::write_output(
                    out_report,
                    ZeStatsReport {
                        abi_size,
                        abi_reserved: 0,
                        resident_owned_bytes: stats.resident_owned_bytes,
                        mapped_bytes: stats.mapped_bytes,
                        mapped_resident_bytes: stats.mapped_resident_bytes,
                        segment_bytes: stats.segment_bytes,
                        active_segment_bytes: stats.active_segment_bytes,
                        active_row_count: stats.active_row_count,
                        tombstone_count: stats.tombstone_count,
                        tombstone_bytes: stats.tombstone_bytes,
                        wal_bytes: stats.wal_bytes,
                        cache_bytes: stats.cache_bytes,
                        temporary_bytes: stats.temporary_bytes,
                        query_pool_bytes: stats.query_pool_bytes,
                        open_files: stats.open_files,
                        active_queries: stats.active_queries,
                        active_snapshot_leases: stats.active_snapshot_leases,
                        phys_footprint: stats.phys_footprint.unwrap_or(0),
                        has_phys_footprint: bool_u32(stats.phys_footprint.is_some()),
                        reserved: 0,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Atomically ingests caller-owned document records. Every const pointer
/// is caller-owned and need only outlive the call. A record with nonzero
/// `text_len` is analyzed into the lexical index with the ingest tokenizer.
/// Not cancellable in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_ingest(
    handle: ZeHandle,
    request: *const ZeIngestRequest,
    out_report: *mut ZeMutationReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_ingest");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                if request.dimension == 0 {
                    return Err(FfiError::invalid("ingest dimension must be nonzero"));
                }
                let vector_bytes = request
                    .dimension
                    .checked_mul(size_of::<f32>())
                    .ok_or_else(|| FfiError::invalid("ingest dimension byte length overflows"))?;
                let records = marshal::read_slice(request.documents, request.document_count)?;
                if records.is_empty() {
                    return Err(FfiError::new(
                        ZeErrorCode::ZeErrEmptyBatch,
                        "ingest batch is empty",
                    ));
                }
                let mut documents = Vec::new();
                documents.try_reserve_exact(records.len()).map_err(|_| {
                    FfiError::new(
                        ZeErrorCode::ZeErrOutOfMemory,
                        "ingest document allocation failed",
                    )
                })?;
                for record in records {
                    let record = marshal::read_struct(record as *const ZeIngestDocument)?;
                    if record.vector_len != request.dimension {
                        return Err(FfiError::new(
                            ZeErrorCode::ZeErrDimensionMismatch,
                            "ingest vector length does not match request dimension",
                        ));
                    }
                    marshal::checked_buffer_len(record.vector_len, size_of::<f32>(), vector_bytes)?;
                    let vector = marshal::copy_slice(record.vector, record.vector_len)?;
                    let metadata = marshal::copy_slice(record.metadata, record.metadata_len)?;
                    let mut document = IngestDocument::new(
                        DocumentVersion::new(doc_id(record.doc_id), Revision::new(record.revision)),
                        vector,
                    )
                    .with_timestamp(record.timestamp)
                    .with_metadata(metadata);
                    if record.text_len != 0 {
                        document = document.with_text(utf8_field(
                            record.text,
                            record.text_len,
                            "document text",
                        )?);
                    }
                    documents.push(document);
                }
                let mut batch = IngestBatch::new(documents);
                if let Some(epoch) = access.epoch {
                    batch = batch.with_epoch(epoch);
                }
                let ack = access.store.ingest(batch).map_err(FfiError::ingest)?;
                marshal::write_output(
                    out_report,
                    ZeMutationReport {
                        abi_size,
                        abi_reserved: 0,
                        sequence: ack.seq().get(),
                        generation: ack.generation(),
                    },
                );
                Ok(())
            }),
        )
    })
}

/// Atomically ingests caller-owned document records with typed schema
/// attributes. Every const pointer is caller-owned and need only outlive the
/// call. Not cancellable in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_upsert(
    handle: ZeHandle,
    request: *const ZeUpsertRequest,
    out_report: *mut ZeMutationReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_upsert");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let record_only =
                    access.store.epoch_identity() == Some(record_only_epoch_identity());
                if record_only && request.dimension > 1 {
                    return Err(FfiError::invalid(
                        "record-only upsert dimension must be zero or one",
                    ));
                }
                if !record_only && request.dimension == 0 {
                    return Err(FfiError::invalid("upsert dimension must be nonzero"));
                }
                let vector_bytes = if record_only {
                    0
                } else {
                    request
                        .dimension
                        .checked_mul(size_of::<f32>())
                        .ok_or_else(|| {
                            FfiError::invalid("upsert dimension byte length overflows")
                        })?
                };
                let records = marshal::read_slice(request.documents, request.document_count)?;
                if records.is_empty() {
                    return Err(FfiError::new(
                        ZeErrorCode::ZeErrEmptyBatch,
                        "upsert batch is empty",
                    ));
                }
                let mut documents = Vec::new();
                documents.try_reserve_exact(records.len()).map_err(|_| {
                    FfiError::new(
                        ZeErrorCode::ZeErrOutOfMemory,
                        "upsert document allocation failed",
                    )
                })?;
                for record in records {
                    let record = marshal::read_struct(record as *const ZeUpsertDocument)?;
                    let document = marshal::read_struct(&record.document)?;
                    let vector = if record_only {
                        if !document.vector.is_null() || document.vector_len != 0 {
                            return Err(FfiError::new(
                                ZeErrorCode::ZeErrNoVectorSpace,
                                "record-only namespace does not accept caller vectors",
                            ));
                        }
                        Vec::from([1.0_f32])
                    } else {
                        if document.vector_len != request.dimension {
                            return Err(FfiError::new(
                                ZeErrorCode::ZeErrDimensionMismatch,
                                "upsert vector length does not match request dimension",
                            ));
                        }
                        marshal::checked_buffer_len(
                            document.vector_len,
                            size_of::<f32>(),
                            vector_bytes,
                        )?;
                        marshal::copy_slice(document.vector, document.vector_len)?
                    };
                    let metadata = marshal::copy_slice(document.metadata, document.metadata_len)?;
                    let attributes =
                        marshal::read_slice(record.attributes, record.attribute_count)?;
                    let mut columns = Vec::new();
                    columns.try_reserve_exact(attributes.len()).map_err(|_| {
                        FfiError::new(
                            ZeErrorCode::ZeErrOutOfMemory,
                            "upsert attribute allocation failed",
                        )
                    })?;
                    for (position, attribute) in attributes.iter().enumerate() {
                        let column = ColumnId::new(attribute.attribute_id);
                        if attributes
                            .iter()
                            .take(position)
                            .any(|prior| prior.attribute_id == attribute.attribute_id)
                        {
                            return Err(FfiError::invalid(format!(
                                "duplicate column {}",
                                column.get()
                            )));
                        }
                        if let Some(value) =
                            parse_attribute_value(access.store.schema(), *attribute)?
                        {
                            columns.push(value);
                        }
                    }
                    let mut ingested = IngestDocument::new(
                        DocumentVersion::new(
                            doc_id(document.doc_id),
                            Revision::new(document.revision),
                        ),
                        vector,
                    )
                    .with_timestamp(document.timestamp)
                    .with_metadata(metadata)
                    .with_columns(columns);
                    if document.text_len != 0 {
                        ingested = ingested.with_text(utf8_field(
                            document.text,
                            document.text_len,
                            "document text",
                        )?);
                    }
                    documents.push(ingested);
                }
                let mut batch = IngestBatch::new(documents);
                if let Some(epoch) = access.epoch {
                    batch = batch.with_epoch(epoch);
                }
                let ack = access.store.ingest(batch).map_err(FfiError::ingest)?;
                marshal::write_output(
                    out_report,
                    ZeMutationReport {
                        abi_size,
                        abi_reserved: 0,
                        sequence: ack.seq().get(),
                        generation: ack.generation(),
                    },
                );
                Ok(())
            }),
        )
    })
}

/// Reads documents by stable id from one pinned generation. Request data is
/// caller-owned for the call; the returned arena must be released exactly once
/// with [`ze_get_result_free`].
#[unsafe(no_mangle)]
pub extern "C" fn ze_get(
    handle: ZeHandle,
    request: *const ZeGetRequest,
    out_result: *mut ZeGetResult,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_get");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_result)?;
                marshal::write_output(out_result, empty_get_result(abi_size));
                let requested_ids = marshal::read_slice(request.ids, request.id_count)?;
                if requested_ids.is_empty() {
                    return Err(FfiError::invalid("get id_count must be nonzero"));
                }
                let include_vector = parse_flag(request.include_vector, "include_vector")?;
                let include_text = parse_flag(request.include_text, "include_text")?;
                let include_metadata = parse_flag(request.include_metadata, "include_metadata")?;
                let include_attributes =
                    parse_flag(request.include_attributes, "include_attributes")?;
                let access = registry::lookup(handle)?;
                let record_only =
                    access.store.epoch_identity() == Some(record_only_epoch_identity());
                let mut fields = DocumentFields::NONE;
                if include_vector && !record_only {
                    fields = fields | DocumentFields::VECTOR;
                }
                if include_text {
                    fields = fields | DocumentFields::TEXT;
                }
                if include_metadata {
                    fields = fields | DocumentFields::METADATA;
                }
                if include_attributes {
                    fields = fields | DocumentFields::ATTRIBUTES;
                }
                let mut ids = Vec::new();
                ids.try_reserve_exact(requested_ids.len()).map_err(|_| {
                    FfiError::new(
                        ZeErrorCode::ZeErrOutOfMemory,
                        "get document-id allocation failed",
                    )
                })?;
                for id in requested_ids {
                    ids.push(doc_id(*id));
                }
                let (generation, documents) = access
                    .store
                    .get_documents_with_generation(&ids, fields)
                    .map_err(FfiError::store)?;
                let (documents, allocation_generation, missing_count) =
                    publish_get_documents(requested_ids, &documents)?;
                marshal::write_output(
                    out_result,
                    ZeGetResult {
                        abi_size,
                        abi_reserved: allocation_generation,
                        documents,
                        document_count: requested_ids.len(),
                        missing_count,
                        generation,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Releases the single arena owned by a get result.
#[unsafe(no_mangle)]
pub extern "C" fn ze_get_result_free(result: *mut ZeGetResult) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_get_result_free");
        finish(
            None,
            (|| {
                if result.is_null() {
                    return Err(FfiError::invalid("get result pointer is null"));
                }
                if result.align_offset(align_of::<ZeGetResult>()) != 0 {
                    return Err(FfiError::invalid("get result pointer is misaligned"));
                }
                let abi_size = marshal::read_abi_size(result);
                marshal::validate_abi_size::<ZeGetResult>(abi_size)?;
                let current = marshal::read_value(result);
                if current.documents.is_null() || current.document_count == 0 {
                    return Err(FfiError::invalid(
                        "get result was already freed or contains no arena",
                    ));
                }
                if current.missing_count > current.document_count {
                    return Err(FfiError::invalid(
                        "get result missing count exceeds document count",
                    ));
                }
                registry::take_registered_result(
                    current.documents.cast::<u64>(),
                    current.document_count,
                    current.abi_reserved,
                )?;
                marshal::write_output(result, empty_get_result(abi_size));
                Ok(())
            })(),
        )
    })
}

/// Atomically tombstones caller-owned document identifiers. Every const
/// pointer is caller-owned and need only outlive the call. Not cancellable
/// in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_delete(
    handle: ZeHandle,
    request: *const ZeDeleteRequest,
    out_report: *mut ZeMutationReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_delete");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let ids = marshal::read_slice(request.doc_ids, request.doc_id_count)?;
                if ids.is_empty() {
                    return Err(FfiError::new(
                        ZeErrorCode::ZeErrEmptyBatch,
                        "delete batch is empty",
                    ));
                }
                let ids = ids.iter().copied().map(doc_id).collect::<Vec<_>>();
                let ack = access
                    .store
                    .delete(DeleteBatch::new(ids))
                    .map_err(FfiError::ingest)?;
                marshal::write_output(
                    out_report,
                    ZeMutationReport {
                        abi_size,
                        abi_reserved: 0,
                        sequence: ack.seq().get(),
                        generation: ack.generation(),
                    },
                );
                Ok(())
            }),
        )
    })
}

fn parse_scan_order(order: i32) -> Result<ScanOrder, FfiError> {
    match order {
        0 => Ok(ScanOrder::Storage),
        1 => Ok(ScanOrder::TimestampAscending),
        2 => Ok(ScanOrder::TimestampDescending),
        _ => Err(FfiError::invalid("scan order discriminant is out of range")),
    }
}

fn parse_scan_cursor(request: ZeScanRequest) -> Result<Option<DocumentScanCursor>, FfiError> {
    if request.cursor_generation == 0 {
        if request.cursor_segment_id != [0; 16]
            || request.cursor_next_row != 0
            || request.cursor_phase != 0
        {
            return Err(FfiError::invalid(
                "scan start cursor fields must be zero when cursor_generation is zero",
            ));
        }
        return Ok(None);
    }
    let source = match request.cursor_phase {
        0 => DocumentScanSource::Sealed(zeppelin_embed::segment::SegmentId::from_bytes(
            request.cursor_segment_id,
        )),
        1 => {
            if request.cursor_segment_id != [0; 16] {
                return Err(FfiError::invalid(
                    "active scan cursor segment id must be zero",
                ));
            }
            DocumentScanSource::Active
        }
        _ => {
            return Err(FfiError::invalid(
                "scan cursor_phase discriminant is out of range",
            ));
        }
    };
    Ok(Some(DocumentScanCursor {
        generation: request.cursor_generation,
        source,
        next_row: request.cursor_next_row,
    }))
}

fn publish_scan_documents(
    documents: Vec<StoredDocument>,
) -> Result<(*mut ZeStoredDocument, u32, usize), FfiError> {
    if documents.is_empty() {
        let mut arena = Vec::<u64>::new();
        arena.try_reserve_exact(1).map_err(|_| {
            FfiError::new(
                ZeErrorCode::ZeErrOutOfMemory,
                "empty scan result arena allocation failed",
            )
        })?;
        arena.push(0);
        let mut arena = arena.into_boxed_slice();
        let arena_pointer = arena.as_mut_ptr();
        let generation = registry::register_result_arena(arena_pointer, arena.len(), 0)?;
        let _raw = Box::into_raw(arena);
        return Ok((arena_pointer.cast::<ZeStoredDocument>(), generation, 0));
    }
    let mut ids = Vec::new();
    ids.try_reserve_exact(documents.len()).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "scan document-id allocation failed",
        )
    })?;
    let mut optional = Vec::new();
    optional.try_reserve_exact(documents.len()).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "scan document allocation failed",
        )
    })?;
    for document in documents {
        ids.push(ffi_doc_id(document.doc_id));
        optional.push(Some(document));
    }
    let (documents, allocation_generation, missing_count) = publish_get_documents(&ids, &optional)?;
    if missing_count != 0 {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrInternal,
            "scan materialization produced a missing document",
        ));
    }
    Ok((documents, allocation_generation, ids.len()))
}

/// Enumerates one ordered, filtered page of live documents. All request and
/// filter pointers are caller-owned for the call; the returned arena must be
/// released exactly once with [`ze_scan_result_free`].
#[unsafe(no_mangle)]
pub extern "C" fn ze_scan(
    handle: ZeHandle,
    request: *const ZeScanRequest,
    out_result: *mut ZeScanResult,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_scan");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_result)?;
                marshal::write_output(out_result, empty_scan_result(abi_size));
                if request.limit == 0 {
                    return Err(FfiError::invalid("scan limit must be nonzero"));
                }
                if request.limit > MAX_DOCUMENT_SCAN_LIMIT {
                    return Err(FfiError::invalid(
                        "scan limit exceeds MAX_DOCUMENT_SCAN_LIMIT",
                    ));
                }
                let order = parse_scan_order(request.order)?;
                let cursor = parse_scan_cursor(request)?;
                let include_vector = parse_flag(request.include_vector, "include_vector")?;
                let include_text = parse_flag(request.include_text, "include_text")?;
                let include_metadata = parse_flag(request.include_metadata, "include_metadata")?;
                let include_attributes =
                    parse_flag(request.include_attributes, "include_attributes")?;
                let has_timestamp_range =
                    parse_flag(request.has_timestamp_range, "has_timestamp_range")?;
                if !has_timestamp_range && (request.start_ts != 0 || request.end_ts != 0) {
                    return Err(FfiError::invalid(
                        "scan timestamp bounds require has_timestamp_range",
                    ));
                }
                let control = query_control_for(request.cancel_token, request.deadline_ns)?;
                let access = registry::lookup(handle)?;
                let predicate = decode_filter(request.filter, access.store.schema())?;
                let record_only =
                    access.store.epoch_identity() == Some(record_only_epoch_identity());
                let mut fields = DocumentFields::NONE;
                if include_vector && !record_only {
                    fields = fields | DocumentFields::VECTOR;
                }
                if include_text {
                    fields = fields | DocumentFields::TEXT;
                }
                if include_metadata {
                    fields = fields | DocumentFields::METADATA;
                }
                if include_attributes {
                    fields = fields | DocumentFields::ATTRIBUTES;
                }
                let mut scan =
                    DocumentScanRequest::new(request.limit, fields, control).with_order(order);
                if let Some(cursor) = cursor {
                    scan = scan.with_cursor(cursor);
                }
                if has_timestamp_range {
                    scan = scan.with_timestamp_range(request.start_ts, request.end_ts);
                }
                if let Some(predicate) = &predicate {
                    scan = scan.with_predicate(predicate);
                }
                let page = access.store.scan_documents(scan).map_err(FfiError::query)?;
                let (documents, allocation_generation, document_count) =
                    publish_scan_documents(page.documents)?;
                let (has_more, next_segment_id, next_row, next_phase) = match page.continuation {
                    Some(DocumentScanCursor {
                        source: DocumentScanSource::Sealed(segment),
                        next_row,
                        ..
                    }) => (1, *segment.as_bytes(), next_row, 0),
                    Some(DocumentScanCursor {
                        source: DocumentScanSource::Active,
                        next_row,
                        ..
                    }) => (1, [0; 16], next_row, 1),
                    None => (0, [0; 16], 0, 0),
                };
                marshal::write_output(
                    out_result,
                    ZeScanResult {
                        abi_size,
                        abi_reserved: allocation_generation,
                        documents,
                        document_count,
                        generation: page.generation,
                        has_more,
                        next_segment_id,
                        next_row,
                        next_phase,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Releases the single arena owned by a scan result.
#[unsafe(no_mangle)]
pub extern "C" fn ze_scan_result_free(result: *mut ZeScanResult) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_scan_result_free");
        finish(
            None,
            (|| {
                if result.is_null() {
                    return Err(FfiError::invalid("scan result pointer is null"));
                }
                if result.align_offset(align_of::<ZeScanResult>()) != 0 {
                    return Err(FfiError::invalid("scan result pointer is misaligned"));
                }
                let abi_size = marshal::read_abi_size(result);
                marshal::validate_abi_size::<ZeScanResult>(abi_size)?;
                let current = marshal::read_value(result);
                if current.documents.is_null() {
                    return Err(FfiError::invalid(
                        "scan result was already freed or contains no arena",
                    ));
                }
                registry::take_registered_result(
                    current.documents.cast::<u64>(),
                    current.document_count,
                    current.abi_reserved,
                )?;
                marshal::write_output(result, empty_scan_result(abi_size));
                Ok(())
            })(),
        )
    })
}

/// Searches active and immutable store state with optional cancellation.
/// The query vector is caller-owned for the call. On success `hits` is
/// callee-owned and must be released exactly once with
/// `ze_search_result_free`. A zeroed result and a second free of the same
/// result object are safe.
#[unsafe(no_mangle)]
pub extern "C" fn ze_search(
    handle: ZeHandle,
    request: *const ZeSearchRequest,
    out_result: *mut ZeSearchResult,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_search");
        run_panic_probe(request);
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_result)?;
                marshal::write_output(out_result, empty_search_result(abi_size));
                if request.k == 0 {
                    return Err(FfiError::invalid("search k must be nonzero"));
                }
                if request.k > ZE_MAX_K {
                    return Err(FfiError::invalid("search k exceeds ZE_MAX_K"));
                }
                if request.dimension == 0 || request.vector_len != request.dimension {
                    return Err(FfiError::new(
                        ZeErrorCode::ZeErrDimensionMismatch,
                        "search vector length does not match a nonzero dimension",
                    ));
                }
                let vector_bytes = request
                    .dimension
                    .checked_mul(size_of::<f32>())
                    .ok_or_else(|| FfiError::invalid("search dimension byte length overflows"))?;
                marshal::checked_buffer_len(request.vector_len, size_of::<f32>(), vector_bytes)?;
                let vector = marshal::copy_slice(request.vector, request.vector_len)?;
                let options = parse_search_options(request)?;
                let control = query_control_for(request.cancel_token, request.deadline_ns)?;
                let access = registry::lookup(handle)?;
                let outcome = access
                    .store
                    .search(SearchRequest::new(&vector), request.k, options, control)
                    .map_err(FfiError::query)?;
                let mut hits = Vec::new();
                hits.try_reserve_exact(outcome.candidates.len())
                    .map_err(|_| {
                        FfiError::new(
                            ZeErrorCode::ZeErrOutOfMemory,
                            "search hit allocation failed",
                        )
                    })?;
                for candidate in outcome.candidates {
                    let (source_kind, segment_id) = match candidate.row_id().source() {
                        RowSource::Active => (0, [0_u8; 16]),
                        RowSource::Sealed(segment) => (1, *segment.as_bytes()),
                    };
                    let (has_document, id, revision) = match candidate.document() {
                        Some(document) => {
                            (1, ffi_doc_id(document.doc_id()), document.revision().get())
                        }
                        None => (0, ZeDocId { high: 0, low: 0 }, 0),
                    };
                    hits.push(ZeSearchHit {
                        source_kind,
                        reserved: 0,
                        segment_id,
                        local_row: candidate.row_id().local_row(),
                        has_document,
                        doc_id: id,
                        revision,
                        score: candidate.score(),
                        reserved_tail: 0,
                    });
                }
                let (hit_pointer, hit_count, allocation_generation) = publish_hits(hits)?;
                marshal::write_output(
                    out_result,
                    ZeSearchResult {
                        abi_size,
                        abi_reserved: allocation_generation,
                        hits: hit_pointer,
                        hit_count,
                        generation: outcome.generation,
                        dims_touched: outcome.stats.dims_touched,
                        bytes_read: outcome.stats.bytes_read,
                        threads_used: usize_u64(outcome.stats.threads_used, "threads_used")?,
                        graph_segments_traversed: usize_u64(
                            outcome.graph_stats.segments_traversed,
                            "graph_segments_traversed",
                        )?,
                        graph_validations: usize_u64(
                            outcome.graph_stats.graph_validations,
                            "graph_validations",
                        )?,
                        graph_entry_seed_discoveries: usize_u64(
                            outcome.graph_stats.entry_seed_discoveries,
                            "graph_entry_seed_discoveries",
                        )?,
                        graph_visited_epoch_clears: usize_u64(
                            outcome.graph_stats.visited_epoch_clears,
                            "graph_visited_epoch_clears",
                        )?,
                        graph_candidates_scored: usize_u64(
                            outcome.graph_stats.candidates_scored,
                            "graph_candidates_scored",
                        )?,
                        graph_candidates_rescored: usize_u64(
                            outcome.graph_stats.candidates_rescored,
                            "graph_candidates_rescored",
                        )?,
                        graph_segments_pruned_by_bound: usize_u64(
                            outcome.graph_stats.segments_pruned_by_bound,
                            "graph_segments_pruned_by_bound",
                        )?,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Runs one structured query: a vector leg, a lexical leg, or exact hybrid
/// fusion of both. Every request pointer is caller-owned for the call. On
/// success `hits` is callee-owned and must be released exactly once with
/// `ze_query_result_free`. A zeroed result and a second free are safe.
#[unsafe(no_mangle)]
pub extern "C" fn ze_query(
    handle: ZeHandle,
    request: *const ZeQueryRequest,
    out_result: *mut ZeQueryResult,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_query");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_result)?;
                marshal::write_output(out_result, empty_query_result(abi_size));
                if request.reserved != 0 {
                    return Err(FfiError::invalid("query reserved field must be zero"));
                }
                if request.k == 0 {
                    return Err(FfiError::invalid("query k must be nonzero"));
                }
                if request.k > ZE_MAX_K {
                    return Err(FfiError::invalid("query k exceeds ZE_MAX_K"));
                }
                if request.vector_len != request.dimension {
                    return Err(FfiError::new(
                        ZeErrorCode::ZeErrDimensionMismatch,
                        "query vector length does not match its declared dimension",
                    ));
                }
                let has_vector = request.vector_len != 0;
                let has_text = request.text_len != 0;
                if !has_vector && !has_text {
                    return Err(FfiError::invalid(
                        "query needs a vector leg, a lexical leg, or both",
                    ));
                }
                let tier = parse_tier(&request)?;
                if !has_vector && tier.is_some() {
                    return Err(FfiError::invalid("a search tier requires a vector leg"));
                }
                let hybrid = parse_hybrid(&request, has_vector && has_text)?;
                let vector = if has_vector {
                    let vector_bytes =
                        request
                            .dimension
                            .checked_mul(size_of::<f32>())
                            .ok_or_else(|| {
                                FfiError::invalid("query dimension byte length overflows")
                            })?;
                    marshal::checked_buffer_len(
                        request.vector_len,
                        size_of::<f32>(),
                        vector_bytes,
                    )?;
                    marshal::copy_slice(request.vector, request.vector_len)?
                } else {
                    Vec::new()
                };
                let lexical = if has_text {
                    Some(analyze_query_text(request.text, request.text_len)?)
                } else {
                    None
                };
                let mut options = SearchOptions::new(ScanOptions {
                    thread_budget: request.thread_budget,
                });
                if let Some(tier) = tier {
                    options = options.with_tier(tier);
                }
                let control = query_control_for(request.cancel_token, request.deadline_ns)?;
                let access = registry::lookup(handle)?;
                let mut result = empty_query_result(abi_size);
                let mut hits = Vec::new();
                match (has_vector, lexical, hybrid) {
                    (true, Some(lexical), Some(mut hybrid)) => {
                        if let Some(epoch) = access.epoch {
                            hybrid = hybrid.with_epoch(epoch);
                        }
                        let outcome = access
                            .store
                            .search_hybrid(
                                SearchRequest::new(&vector),
                                &lexical,
                                &hybrid,
                                options,
                                control,
                            )
                            .map_err(FfiError::fusion)?;
                        result.mode = 2;
                        fill_diagnostics(&mut result, &outcome.diagnostics)?;
                        result.generation = outcome.generation;
                        hits.try_reserve_exact(outcome.hits.len()).map_err(|_| {
                            FfiError::new(
                                ZeErrorCode::ZeErrOutOfMemory,
                                "query hit allocation failed",
                            )
                        })?;
                        for hit in outcome.hits {
                            hits.push(ZeQueryHit {
                                has_document: 1,
                                has_revision: 0,
                                doc_id: ffi_doc_id(hit.key),
                                revision: 0,
                                score: hit.fused_score,
                                has_vector_score: bool_u32(hit.vector_squared_l2.is_some()),
                                has_lexical_score: bool_u32(hit.lexical_bm25.is_some()),
                                vector_squared_l2: hit.vector_squared_l2.unwrap_or(0.0),
                                lexical_bm25: hit.lexical_bm25.unwrap_or(0.0),
                            });
                        }
                    }
                    (false, Some(lexical), None) => {
                        let outcome = access
                            .store
                            .search_lexical(&lexical, request.k, control)
                            .map_err(FfiError::lexical)?;
                        result.mode = 1;
                        fill_diagnostics(&mut result, &outcome.diagnostics)?;
                        result.generation = outcome.generation;
                        hits.try_reserve_exact(outcome.candidates.len())
                            .map_err(|_| {
                                FfiError::new(
                                    ZeErrorCode::ZeErrOutOfMemory,
                                    "query hit allocation failed",
                                )
                            })?;
                        for candidate in outcome.candidates {
                            hits.push(ZeQueryHit {
                                has_document: 1,
                                has_revision: 1,
                                doc_id: ffi_doc_id(candidate.document.doc_id()),
                                revision: candidate.document.revision().get(),
                                score: candidate.score,
                                has_vector_score: 0,
                                has_lexical_score: 1,
                                vector_squared_l2: 0.0,
                                lexical_bm25: candidate.score,
                            });
                        }
                    }
                    (true, None, None) => {
                        let outcome = access
                            .store
                            .search(SearchRequest::new(&vector), request.k, options, control)
                            .map_err(FfiError::query)?;
                        result.mode = 0;
                        fill_diagnostics(&mut result, &outcome.diagnostics)?;
                        result.generation = outcome.generation;
                        hits.try_reserve_exact(outcome.candidates.len())
                            .map_err(|_| {
                                FfiError::new(
                                    ZeErrorCode::ZeErrOutOfMemory,
                                    "query hit allocation failed",
                                )
                            })?;
                        for candidate in outcome.candidates {
                            let (has_document, doc_id, revision) = match candidate.document() {
                                Some(document) => {
                                    (1, ffi_doc_id(document.doc_id()), document.revision().get())
                                }
                                None => (0, ZeDocId { high: 0, low: 0 }, 0),
                            };
                            let score = f64::from(candidate.score());
                            hits.push(ZeQueryHit {
                                has_document,
                                has_revision: has_document,
                                doc_id,
                                revision,
                                score,
                                has_vector_score: 1,
                                has_lexical_score: 0,
                                vector_squared_l2: -score,
                                lexical_bm25: 0.0,
                            });
                        }
                    }
                    _ => {
                        return Err(FfiError::new(
                            ZeErrorCode::ZeErrInternal,
                            "query leg dispatch reached an impossible combination",
                        ));
                    }
                }
                let (hit_pointer, hit_count, allocation_generation) = publish_hits(hits)?;
                result.abi_reserved = allocation_generation;
                result.hits = hit_pointer;
                result.hit_count = hit_count;
                marshal::write_output(out_result, result);
                Ok(())
            })(),
        )
    })
}

/// Releases a callee-owned query hit array; a zeroed result is a successful
/// no-op. `result` is caller-owned; only its `hits` allocation is released.
#[unsafe(no_mangle)]
pub extern "C" fn ze_query_result_free(result: *mut ZeQueryResult) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(
            None,
            (|| {
                if result.is_null() {
                    return Err(FfiError::invalid("query result pointer is null"));
                }
                if result.align_offset(align_of::<ZeQueryResult>()) != 0 {
                    return Err(FfiError::invalid("query result pointer is misaligned"));
                }
                let abi_size = marshal::read_abi_size(result);
                if abi_size == 0 {
                    let zeroed = marshal::read_value(result);
                    if zeroed.hits.is_null() && zeroed.hit_count == 0 {
                        return Ok(());
                    }
                    return Err(FfiError::invalid(
                        "zero-sized query result contains an allocation",
                    ));
                }
                marshal::validate_abi_size::<ZeQueryResult>(abi_size)?;
                let current = marshal::read_value(result);
                registry::take_result(current.hits, current.hit_count, current.abi_reserved)?;
                marshal::write_output(result, empty_query_result(abi_size));
                Ok(())
            })(),
        )
    })
}

/// Seals the active segment. Cancellable through `request.cancel_token`.
#[unsafe(no_mangle)]
pub extern "C" fn ze_seal(
    handle: ZeHandle,
    request: *const ZeSealRequest,
    out_report: *mut ZeGenerationReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_seal");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let cancel = if request.cancel_token == 0 {
                    None
                } else {
                    Some(registry::lookup_cancel(request.cancel_token)?)
                };
                let generation = match cancel.as_ref() {
                    Some(cancel) => access.store.seal_with_cancel(cancel),
                    None => access.store.seal(),
                }
                .map_err(FfiError::store)?;
                marshal::write_output(
                    out_report,
                    ZeGenerationReport {
                        abi_size,
                        abi_reserved: 0,
                        generation,
                    },
                );
                Ok(())
            }),
        )
    })
}

fn write_partition_report(
    output: *mut ZePartitionReport,
    abi_size: u32,
    report: zeppelin_embed::ingest::DropPartitionReport,
) -> Result<(), FfiError> {
    marshal::write_output(
        output,
        ZePartitionReport {
            abi_size,
            abi_reserved: 0,
            generation: report.generation(),
            segments_dropped: usize_u64(report.segments_dropped().len(), "segments_dropped")?,
            bytes_reclaimed: report.bytes_reclaimed(),
            straddlers_skipped: usize_u64(report.straddlers_skipped().len(), "straddlers_skipped")?,
            is_no_op: bool_u32(report.is_no_op()),
            reserved: 0,
        },
    );
    Ok(())
}

/// Drops immutable segments wholly contained by a timestamp range. Not
/// cancellable in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_drop_partition(
    handle: ZeHandle,
    request: *const ZeDropPartitionRequest,
    out_report: *mut ZePartitionReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_drop_partition");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                if request.start_ts >= request.end_ts {
                    return Err(FfiError::invalid(
                        "partition range must be a nonempty half-open interval",
                    ));
                }
                let report = access
                    .store
                    .drop_partition(request.start_ts..request.end_ts)
                    .map_err(FfiError::store)?;
                write_partition_report(out_report, abi_size, report)
            }),
        )
    })
}

/// Applies a positive retention window at a caller-supplied timestamp. Not
/// cancellable in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_apply_retention(
    handle: ZeHandle,
    request: *const ZeRetentionRequest,
    out_report: *mut ZePartitionReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_apply_retention");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let policy = zeppelin_embed::ingest::RetentionPolicy::new(request.window)
                    .map_err(|error| FfiError::invalid(error.to_string()))?;
                let report = access
                    .store
                    .apply_retention(policy, request.now_ts)
                    .map_err(FfiError::store)?;
                write_partition_report(out_report, abi_size, report)
            }),
        )
    })
}

/// Schedules physical removal of document ids and returns an opaque token.
/// Not cancellable in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_purge(
    handle: ZeHandle,
    request: *const ZePurgeRequest,
    out_report: *mut ZePurgeTokenReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_purge");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let ids = marshal::read_slice(request.doc_ids, request.doc_id_count)?;
                if ids.is_empty() {
                    return Err(FfiError::new(
                        ZeErrorCode::ZeErrEmptyBatch,
                        "purge id batch is empty",
                    ));
                }
                let ids = ids.iter().copied().map(doc_id).collect::<Vec<_>>();
                let token = access.store.purge(&ids).map_err(FfiError::purge)?;
                let token_id = token.id();
                access
                    .purge_tokens
                    .lock()
                    .map_err(|_| {
                        FfiError::new(
                            ZeErrorCode::ZeErrSynchronization,
                            "purge-token registry mutex is poisoned",
                        )
                    })?
                    .insert(token_id, token.clone());
                marshal::write_output(
                    out_report,
                    ZePurgeTokenReport {
                        abi_size,
                        abi_reserved: 0,
                        token_id,
                        generation: token.generation(),
                        unknown_id_count: usize_u64(token.unknown_ids().len(), "unknown_id_count")?,
                        is_no_op: bool_u32(token.is_no_op()),
                        reserved: 0,
                    },
                );
                Ok(())
            }),
        )
    })
}

/// Waits until a scheduled purge has removed every reachable physical byte.
/// Not cancellable in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_await_physical_purge(
    handle: ZeHandle,
    request: *const ZeAwaitPurgeRequest,
    out_report: *mut ZePurgeReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_await_physical_purge");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let token = access
                    .purge_tokens
                    .lock()
                    .map_err(|_| {
                        FfiError::new(
                            ZeErrorCode::ZeErrSynchronization,
                            "purge-token registry mutex is poisoned",
                        )
                    })?
                    .get(&request.token_id)
                    .cloned()
                    .ok_or_else(|| FfiError::invalid("purge token is unknown for this handle"))?;
                let report = access
                    .store
                    .await_physical_purge(token)
                    .map_err(FfiError::purge)?;
                access
                    .purge_tokens
                    .lock()
                    .map_err(|_| {
                        FfiError::new(
                            ZeErrorCode::ZeErrSynchronization,
                            "purge-token registry mutex is poisoned",
                        )
                    })?
                    .remove(&request.token_id);
                marshal::write_output(
                    out_report,
                    ZePurgeReport {
                        abi_size,
                        abi_reserved: 0,
                        generation: report.generation(),
                        segments_rewritten: usize_u64(
                            report.segments_rewritten(),
                            "segments_rewritten",
                        )?,
                        unknown_id_count: usize_u64(
                            report.unknown_ids().len(),
                            "unknown_id_count",
                        )?,
                        wal_rewritten: bool_u32(report.wal_rewritten()),
                        is_no_op: bool_u32(report.is_no_op()),
                    },
                );
                Ok(())
            }),
        )
    })
}

/// Runs due tier transitions within caller-supplied work budgets. Not
/// cancellable in v1; the engine offers no token here.
#[unsafe(no_mangle)]
pub extern "C" fn ze_maintain(
    handle: ZeHandle,
    request: *const ZeMaintainRequest,
    out_report: *mut ZeMaintainReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_maintain");
        finish(
            Some(handle),
            registry::with_writer(handle, |access| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let report = access.store.maintain(MaintenanceBudget {
                    wall_time: Duration::from_nanos(request.wall_time_ns),
                    bytes: request.bytes,
                });
                let status = match report.status {
                    MaintenanceStatus::Complete => 0,
                    MaintenanceStatus::BudgetExhausted => 1,
                    MaintenanceStatus::Failed(error) => {
                        return Err(FfiError::maintenance(error));
                    }
                };
                marshal::write_output(
                    out_report,
                    ZeMaintainReport {
                        abi_size,
                        abi_reserved: 0,
                        graphs_built: report.graphs_built,
                        bytes_consumed: report.bytes_consumed,
                        checkpoints_resumed: report.checkpoints_resumed,
                        status,
                        reserved: 0,
                    },
                );
                Ok(())
            }),
        )
    })
}

/// Copies the per-handle or process-global last error into caller-owned
/// memory. `buffer` and `written` are caller-owned. Capacity zero with a
/// non-null `written` is a legal size probe. Pass handle zero for pre-handle
/// or global errors. This is the sole store-handle accessor that remains
/// usable after poisoning.
#[unsafe(no_mangle)]
pub extern "C" fn ze_last_error_message(
    handle: ZeHandle,
    buffer: *mut c_char,
    capacity: usize,
    written: *mut usize,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::ZeErrPanic, {
        finish(
            if handle == 0 { None } else { Some(handle) },
            (|| {
                scalar_output(written)?;
                let message = handle_error(handle)?;
                marshal::write_scalar(written, message.len());
                if capacity == 0 {
                    return Ok(());
                }
                if buffer.is_null() {
                    return Err(FfiError::invalid(
                        "last-error buffer is null with nonzero capacity",
                    ));
                }
                let required = message
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| FfiError::invalid("last-error length overflow"))?;
                if capacity < required {
                    return Err(FfiError::invalid("last-error buffer capacity is too small"));
                }
                marshal::copy_nul_terminated(&message, buffer);
                Ok(())
            })(),
        )
    })
}

/// Returns a static NUL-terminated symbolic name for one numeric error code.
/// The string must never be freed.
#[unsafe(no_mangle)]
pub extern "C" fn ze_error_code_name(code: i32) -> *const c_char {
    ffi_entry!(None, std::ptr::null(), {
        let bytes: &'static [u8] = match code {
            0 => b"ZE_OK\0",
            1 => b"ZE_ERR_INVALID_ARGUMENT\0",
            2 => b"ZE_ERR_INVALID_HANDLE\0",
            3 => b"ZE_ERR_CLOSED\0",
            4 => b"ZE_ERR_CLOSING\0",
            5 => b"ZE_ERR_POISONED\0",
            6 => b"ZE_ERR_PANIC\0",
            7 => b"ZE_ERR_BUSY\0",
            8 => b"ZE_ERR_STORE_BUSY\0",
            9 => b"ZE_ERR_IO\0",
            10 => b"ZE_ERR_CORRUPT\0",
            11 => b"ZE_ERR_UNSUPPORTED\0",
            12 => b"ZE_ERR_CANCELLED\0",
            13 => b"ZE_ERR_TIMEOUT\0",
            14 => b"ZE_ERR_OUT_OF_MEMORY\0",
            15 => b"ZE_ERR_BUDGET_EXCEEDED\0",
            16 => b"ZE_ERR_EMPTY_BATCH\0",
            17 => b"ZE_ERR_STALE_REVISION\0",
            18 => b"ZE_ERR_DIMENSION_MISMATCH\0",
            19 => b"ZE_ERR_NOT_FOUND\0",
            20 => b"ZE_ERR_SYNCHRONIZATION\0",
            21 => b"ZE_ERR_ACCESS_MODE\0",
            22 => b"ZE_ERR_INTERNAL\0",
            23 => b"ZE_ERR_EPOCH_MISMATCH\0",
            24 => b"ZE_ERR_EPOCH_UNDECLARED\0",
            25 => b"ZE_ERR_EPOCH_UNSTAMPED\0",
            26 => b"ZE_ERR_EPOCH_INCOMPLETE\0",
            27 => b"ZE_ERR_EPOCH_PUBLISHED\0",
            28 => b"ZE_ERR_UNSEALED_WRITES\0",
            29 => b"ZE_ERR_BUNDLE\0",
            30 => b"ZE_ERR_MODEL\0",
            31 => b"ZE_ERR_PIPELINE\0",
            32 => b"ZE_ERR_SCAN_STALE\0",
            33 => b"ZE_ERR_SCHEMA_MISMATCH\0",
            34 => b"ZE_ERR_NO_VECTOR_SPACE\0",
            _ => b"ZE_ERR_UNKNOWN\0",
        };
        bytes.as_ptr().cast::<c_char>()
    })
}

/// Creates an active generation-tagged cancellation token.
#[unsafe(no_mangle)]
pub extern "C" fn ze_cancel_token_create(out_token: *mut ZeCancelToken) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(
            None,
            (|| {
                scalar_output(out_token)?;
                let token = registry::insert_cancel(CancelToken::new())?;
                marshal::write_scalar(out_token, token);
                Ok(())
            })(),
        )
    })
}

/// Requests cancellation; repeated requests are harmless.
#[unsafe(no_mangle)]
pub extern "C" fn ze_cancel_token_cancel(token: ZeCancelToken) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(
            None,
            registry::lookup_cancel(token).map(|token| token.cancel()),
        )
    })
}

/// Releases a cancellation token; a stale second free returns `Closed`.
#[unsafe(no_mangle)]
pub extern "C" fn ze_cancel_token_free(token: ZeCancelToken) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(None, registry::free_cancel(token))
    })
}

/// Releases a callee-owned hit array; a zeroed result is a successful no-op.
/// `result` is caller-owned; only its `hits` allocation is released.
#[unsafe(no_mangle)]
pub extern "C" fn ze_search_result_free(result: *mut ZeSearchResult) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        finish(
            None,
            (|| {
                if result.is_null() {
                    return Err(FfiError::invalid("search result pointer is null"));
                }
                if result.align_offset(align_of::<ZeSearchResult>()) != 0 {
                    return Err(FfiError::invalid("search result pointer is misaligned"));
                }
                let abi_size = marshal::read_abi_size(result);
                if abi_size == 0 {
                    let zeroed = marshal::read_value(result);
                    if zeroed.hits.is_null() && zeroed.hit_count == 0 {
                        return Ok(());
                    }
                    return Err(FfiError::invalid(
                        "zero-sized search result contains an allocation",
                    ));
                }
                marshal::validate_abi_size::<ZeSearchResult>(abi_size)?;
                let current = marshal::read_value(result);
                registry::take_result(current.hits, current.hit_count, current.abi_reserved)?;
                marshal::write_output(result, empty_search_result(abi_size));
                Ok(())
            })(),
        )
    })
}

const _: () = {
    assert!(size_of::<ZeFilter>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeGetRequest>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeGetResult>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeOpenRequest>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeSearchResult>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeScanRequest>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeScanResult>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeEpochRequest>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeQueryRequest>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeQueryResult>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeNamespaceOpenRequest>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeNamespaceListRequest>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeNamespaceListResult>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
};
