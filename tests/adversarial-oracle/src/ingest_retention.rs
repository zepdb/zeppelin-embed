//! Independent, std-only expected-value oracle for ingest and retention.
//!
//! This module consumes primitive fixture facts and public Store observations.
//! It deliberately has no dependency on production crates or the adversarial
//! runner/model.

use std::collections::BTreeMap;

pub const I20_CHECKER_ID: &str = "I20.batch-atomicity.v1";
pub const I21_CHECKER_ID: &str = "I21.seal-multiset.v1";
pub const I22_CHECKER_ID: &str = "I22.retention-boundary.v1";
pub const I23_CHECKER_ID: &str = "I23.purge-proof.v1";
pub const ORACLE_CONTRACT_VERSION: &str = "ingest-retention-v1";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DocumentFact {
    pub doc_id: u128,
    pub revision: u64,
    pub timestamp_witness: Vec<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AckFact {
    pub seq: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum I20Outcome {
    Commit,
    Reject { kind: String, detail: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryExpected {
    pub ack: AckFact,
    pub live: Vec<DocumentFact>,
    pub wal_records_appended: u64,
    pub generation_delta: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I20Expected {
    pub baseline: Vec<DocumentFact>,
    pub submitted: Vec<DocumentFact>,
    pub outcome: I20Outcome,
    pub final_live: Vec<DocumentFact>,
    pub expected_ack: Option<AckFact>,
    pub expected_generation_delta: u64,
    pub expected_reopen_live: Vec<DocumentFact>,
    pub retry: Option<RetryExpected>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IngestResultFact {
    Committed(AckFact),
    Rejected { kind: String, detail: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I20StatsFact {
    pub active_rows: u64,
    pub tombstones: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I20Observed {
    pub typed_result: IngestResultFact,
    pub before_live: Vec<DocumentFact>,
    pub immediate_live: Vec<DocumentFact>,
    pub stats: I20StatsFact,
    pub post_reopen_live: Vec<DocumentFact>,
    pub ack: Option<AckFact>,
    pub generation_before: u64,
    pub generation_after: u64,
    pub retry_ack: Option<AckFact>,
    pub retry_live: Option<Vec<DocumentFact>>,
    pub retry_wal_records_appended: Option<u64>,
    pub retry_generation_delta: Option<u64>,
    pub receipt_digest: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I21Expected {
    pub live_before: Vec<DocumentFact>,
    pub live_after: Vec<DocumentFact>,
    pub active_rows_before: u64,
    pub active_rows_after: u64,
    pub generation_delta: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SealResultFact {
    Committed { generation: u64 },
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I21Observed {
    pub seal_result: SealResultFact,
    pub live_before: Vec<DocumentFact>,
    pub cancelled_live: Option<Vec<DocumentFact>>,
    pub cancelled_active_rows: Option<u64>,
    pub cancelled_generation: Option<u64>,
    pub cancelled_orphan_paths: Option<Vec<String>>,
    pub retry_seal_generation: Option<u64>,
    pub live_after: Vec<DocumentFact>,
    pub live_after_reopen: Vec<DocumentFact>,
    pub active_rows_before: u64,
    pub active_rows_after: u64,
    pub generation_before: u64,
    pub generation_after: u64,
    pub orphan_paths: Vec<String>,
    pub receipt_digest: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I22PartitionInput {
    pub label: String,
    pub rows: Vec<(i64, DocumentFact)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I22PartitionBound {
    pub label: String,
    pub min_ts: i64,
    pub max_ts: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I22RangeFact {
    pub start: i64,
    pub end: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I22Expected {
    pub now: i64,
    pub window: i64,
    pub cutoff: i64,
    pub drop_range: I22RangeFact,
    pub partition_bounds: Vec<I22PartitionBound>,
    pub expected_dropped_labels: Vec<String>,
    pub expected_straddler_labels: Vec<String>,
    pub expected_retained_live: Vec<DocumentFact>,
    pub expected_active_control: DocumentFact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I22Observed {
    pub supplied_now: i64,
    pub observed_cutoff: i64,
    pub observed_drop_range: I22RangeFact,
    pub generation_before: u64,
    pub report_generation: u64,
    pub manifest_committed: bool,
    pub dropped_labels: Vec<String>,
    pub straddler_labels: Vec<String>,
    pub bytes_reclaimed: u64,
    pub live_after: Vec<DocumentFact>,
    pub live_after_reopen: Vec<DocumentFact>,
    pub active_control_present: bool,
    pub receipt_digest: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I23TargetLocation {
    Active,
    Sealed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I23SentinelPattern {
    pub index: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct I23SentinelHit {
    pub relative_path: String,
    pub offset: u64,
    pub sentinel_index: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I23Expected {
    pub target: DocumentFact,
    pub target_location: I23TargetLocation,
    pub sentinel_patterns: Vec<I23SentinelPattern>,
    pub expected_logically_live: Vec<DocumentFact>,
    pub expected_physically_live: Vec<DocumentFact>,
    pub token_no_op: bool,
    pub intent_after_completion: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I23DeleteResultFact {
    Committed(AckFact),
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I23PurgeTokenFact {
    pub token_id: u64,
    pub no_op: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I23AwaitResultFact {
    pub completed: bool,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I23GenerationFacts {
    pub before_delete: u64,
    pub after_delete: u64,
    pub after_purge: u64,
    pub after_reopen: u64,
    pub after_second_reopen: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I23Observed {
    pub delete_result: I23DeleteResultFact,
    pub logical_search: Vec<DocumentFact>,
    pub pre_purge_hits: Vec<I23SentinelHit>,
    pub purge_token: I23PurgeTokenFact,
    pub fault_error: Option<String>,
    pub await_result: I23AwaitResultFact,
    pub post_purge_hits: Vec<I23SentinelHit>,
    pub intent_present: bool,
    pub immediate_live: Vec<DocumentFact>,
    pub reopen_live: Vec<DocumentFact>,
    pub second_reopen_live: Vec<DocumentFact>,
    pub generation_facts: I23GenerationFacts,
    pub receipt_digest: Option<u64>,
}

pub const INGEST_CANONICAL_VERSION: u32 = 1;
const INGEST_CANONICAL_MAX_ITEMS: usize = 1 << 20;
const INGEST_CANONICAL_MAX_BYTES: usize = 1 << 24;

/// Stable first disagreement produced by a retained independent comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngestFirstDifference {
    pub checker_id: &'static str,
    pub path: String,
    pub expected: String,
    pub observed: String,
}

/// Canonical input/observation identity retained beside one oracle row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleAttestation {
    pub checker_id: &'static str,
    pub canonical_version: u32,
    pub input_digest: u64,
    pub observed_digest: u64,
    pub input_bytes: Vec<u8>,
    pub observed_bytes: Vec<u8>,
    pub first_difference: Option<IngestFirstDifference>,
}

/// Result of decoding and rerunning one retained canonical comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngestCanonicalReplay {
    pub checker_id: &'static str,
    pub input_digest: u64,
    pub observed_digest: u64,
    pub first_difference: Option<IngestFirstDifference>,
}

#[must_use]
pub fn canonical_digest(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[must_use]
pub fn canonical_i20_input_bytes(expected: &I20Expected) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"ingest-retention/I20/input/v1");
    canonical.documents(&expected.baseline);
    canonical.documents(&expected.submitted);
    canonical.i20_outcome(&expected.outcome);
    canonical.documents(&expected.final_live);
    canonical.optional_ack(expected.expected_ack);
    canonical.u64(expected.expected_generation_delta);
    canonical.documents(&expected.expected_reopen_live);
    canonical.optional_retry(expected.retry.as_ref());
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i20_observed_bytes(observed: &I20Observed) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"ingest-retention/I20/observed/v1");
    canonical.ingest_result(&observed.typed_result);
    canonical.documents(&observed.before_live);
    canonical.documents(&observed.immediate_live);
    canonical.u64(observed.stats.active_rows);
    canonical.u64(observed.stats.tombstones);
    canonical.documents(&observed.post_reopen_live);
    canonical.optional_ack(observed.ack);
    canonical.u64(observed.generation_before);
    canonical.u64(observed.generation_after);
    canonical.optional_ack(observed.retry_ack);
    canonical.optional_documents(observed.retry_live.as_deref());
    canonical.optional_u64(observed.retry_wal_records_appended);
    canonical.optional_u64(observed.retry_generation_delta);
    canonical.optional_u64(observed.receipt_digest);
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i21_input_bytes(expected: &I21Expected) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"ingest-retention/I21/input/v1");
    canonical.documents(&expected.live_before);
    canonical.documents(&expected.live_after);
    canonical.u64(expected.active_rows_before);
    canonical.u64(expected.active_rows_after);
    canonical.u64(expected.generation_delta);
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i21_observed_bytes(observed: &I21Observed) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"ingest-retention/I21/observed/v1");
    canonical.seal_result(observed.seal_result);
    canonical.documents(&observed.live_before);
    canonical.optional_documents(observed.cancelled_live.as_deref());
    canonical.optional_u64(observed.cancelled_active_rows);
    canonical.optional_u64(observed.cancelled_generation);
    canonical.optional_strings(observed.cancelled_orphan_paths.as_deref());
    canonical.optional_u64(observed.retry_seal_generation);
    canonical.documents(&observed.live_after);
    canonical.documents(&observed.live_after_reopen);
    canonical.u64(observed.active_rows_before);
    canonical.u64(observed.active_rows_after);
    canonical.u64(observed.generation_before);
    canonical.u64(observed.generation_after);
    canonical.strings(&observed.orphan_paths);
    canonical.optional_u64(observed.receipt_digest);
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i22_input_bytes(expected: &I22Expected) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"ingest-retention/I22/input/v1");
    canonical.i64(expected.now);
    canonical.i64(expected.window);
    canonical.i64(expected.cutoff);
    canonical.range(expected.drop_range);
    canonical.len(expected.partition_bounds.len());
    for partition in &expected.partition_bounds {
        canonical.string(&partition.label);
        canonical.i64(partition.min_ts);
        canonical.i64(partition.max_ts);
    }
    canonical.strings(&expected.expected_dropped_labels);
    canonical.strings(&expected.expected_straddler_labels);
    canonical.documents(&expected.expected_retained_live);
    canonical.document(&expected.expected_active_control);
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i22_observed_bytes(observed: &I22Observed) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"ingest-retention/I22/observed/v1");
    canonical.i64(observed.supplied_now);
    canonical.i64(observed.observed_cutoff);
    canonical.range(observed.observed_drop_range);
    canonical.u64(observed.generation_before);
    canonical.u64(observed.report_generation);
    canonical.boolean(observed.manifest_committed);
    canonical.strings(&observed.dropped_labels);
    canonical.strings(&observed.straddler_labels);
    canonical.u64(observed.bytes_reclaimed);
    canonical.documents(&observed.live_after);
    canonical.documents(&observed.live_after_reopen);
    canonical.boolean(observed.active_control_present);
    canonical.optional_u64(observed.receipt_digest);
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i23_input_bytes(expected: &I23Expected) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"ingest-retention/I23/input/v1");
    canonical.document(&expected.target);
    canonical.target_location(expected.target_location);
    canonical.len(expected.sentinel_patterns.len());
    for pattern in &expected.sentinel_patterns {
        canonical.u64(pattern.index);
        canonical.blob(&pattern.bytes);
    }
    canonical.documents(&expected.expected_logically_live);
    canonical.documents(&expected.expected_physically_live);
    canonical.boolean(expected.token_no_op);
    canonical.boolean(expected.intent_after_completion);
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i23_observed_bytes(observed: &I23Observed) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"ingest-retention/I23/observed/v1");
    canonical.delete_result(observed.delete_result);
    canonical.documents(&observed.logical_search);
    canonical.sentinel_hits(&observed.pre_purge_hits);
    canonical.u64(observed.purge_token.token_id);
    canonical.boolean(observed.purge_token.no_op);
    canonical.optional_string(observed.fault_error.as_deref());
    canonical.boolean(observed.await_result.completed);
    canonical.u64(observed.await_result.generation);
    canonical.sentinel_hits(&observed.post_purge_hits);
    canonical.boolean(observed.intent_present);
    canonical.documents(&observed.immediate_live);
    canonical.documents(&observed.reopen_live);
    canonical.documents(&observed.second_reopen_live);
    canonical.u64(observed.generation_facts.before_delete);
    canonical.u64(observed.generation_facts.after_delete);
    canonical.u64(observed.generation_facts.after_purge);
    canonical.u64(observed.generation_facts.after_reopen);
    canonical.u64(observed.generation_facts.after_second_reopen);
    canonical.optional_u64(observed.receipt_digest);
    canonical.into_bytes()
}

#[must_use]
pub fn attest_i20(expected: &I20Expected, observed: &I20Observed) -> OracleAttestation {
    let input_bytes = canonical_i20_input_bytes(expected);
    let observed_bytes = canonical_i20_observed_bytes(observed);
    OracleAttestation {
        checker_id: I20_CHECKER_ID,
        canonical_version: INGEST_CANONICAL_VERSION,
        input_digest: canonical_digest(&input_bytes),
        observed_digest: canonical_digest(&observed_bytes),
        input_bytes,
        observed_bytes,
        first_difference: first_i20_difference(expected, observed),
    }
}

#[must_use]
pub fn attest_i21(expected: &I21Expected, observed: &I21Observed) -> OracleAttestation {
    let input_bytes = canonical_i21_input_bytes(expected);
    let observed_bytes = canonical_i21_observed_bytes(observed);
    OracleAttestation {
        checker_id: I21_CHECKER_ID,
        canonical_version: INGEST_CANONICAL_VERSION,
        input_digest: canonical_digest(&input_bytes),
        observed_digest: canonical_digest(&observed_bytes),
        input_bytes,
        observed_bytes,
        first_difference: first_i21_difference(expected, observed),
    }
}

#[must_use]
pub fn attest_i22(expected: &I22Expected, observed: &I22Observed) -> OracleAttestation {
    let input_bytes = canonical_i22_input_bytes(expected);
    let observed_bytes = canonical_i22_observed_bytes(observed);
    OracleAttestation {
        checker_id: I22_CHECKER_ID,
        canonical_version: INGEST_CANONICAL_VERSION,
        input_digest: canonical_digest(&input_bytes),
        observed_digest: canonical_digest(&observed_bytes),
        input_bytes,
        observed_bytes,
        first_difference: first_i22_difference(expected, observed),
    }
}

#[must_use]
pub fn attest_i23(expected: &I23Expected, observed: &I23Observed) -> OracleAttestation {
    let input_bytes = canonical_i23_input_bytes(expected);
    let observed_bytes = canonical_i23_observed_bytes(observed);
    OracleAttestation {
        checker_id: I23_CHECKER_ID,
        canonical_version: INGEST_CANONICAL_VERSION,
        input_digest: canonical_digest(&input_bytes),
        observed_digest: canonical_digest(&observed_bytes),
        input_bytes,
        observed_bytes,
        first_difference: first_i23_difference(expected, observed),
    }
}

fn first_i20_difference(
    expected: &I20Expected,
    observed: &I20Observed,
) -> Option<IngestFirstDifference> {
    compare_i20(expected, observed)
        .err()
        .map(|error| IngestFirstDifference {
            checker_id: I20_CHECKER_ID,
            path: "comparison".to_owned(),
            expected: "exact comparison succeeded".to_owned(),
            observed: error,
        })
}

fn first_i21_difference(
    expected: &I21Expected,
    observed: &I21Observed,
) -> Option<IngestFirstDifference> {
    compare_i21(expected, observed)
        .err()
        .map(|error| IngestFirstDifference {
            checker_id: I21_CHECKER_ID,
            path: "comparison".to_owned(),
            expected: "exact comparison succeeded".to_owned(),
            observed: error,
        })
}

fn first_i22_difference(
    expected: &I22Expected,
    observed: &I22Observed,
) -> Option<IngestFirstDifference> {
    compare_i22(expected, observed)
        .err()
        .map(|error| IngestFirstDifference {
            checker_id: I22_CHECKER_ID,
            path: "comparison".to_owned(),
            expected: "exact comparison succeeded".to_owned(),
            observed: error,
        })
}

fn first_i23_difference(
    expected: &I23Expected,
    observed: &I23Observed,
) -> Option<IngestFirstDifference> {
    compare_i23(expected, observed)
        .err()
        .map(|error| IngestFirstDifference {
            checker_id: I23_CHECKER_ID,
            path: "comparison".to_owned(),
            expected: "exact comparison succeeded".to_owned(),
            observed: error,
        })
}

/// Decode retained canonical bytes and rerun the selected independent checker.
pub fn replay_canonical_comparison(
    checker_id: &str,
    input_bytes: &[u8],
    observed_bytes: &[u8],
) -> Result<IngestCanonicalReplay, String> {
    let (checker_id, first_difference) = match checker_id {
        I20_CHECKER_ID => {
            let expected = decode_i20_input(input_bytes)?;
            let observed = decode_i20_observed(observed_bytes)?;
            if canonical_i20_input_bytes(&expected) != input_bytes
                || canonical_i20_observed_bytes(&observed) != observed_bytes
            {
                return Err("ingest-retention I20 retained bytes are not canonical".to_owned());
            }
            (I20_CHECKER_ID, first_i20_difference(&expected, &observed))
        }
        I21_CHECKER_ID => {
            let expected = decode_i21_input(input_bytes)?;
            let observed = decode_i21_observed(observed_bytes)?;
            if canonical_i21_input_bytes(&expected) != input_bytes
                || canonical_i21_observed_bytes(&observed) != observed_bytes
            {
                return Err("ingest-retention I21 retained bytes are not canonical".to_owned());
            }
            (I21_CHECKER_ID, first_i21_difference(&expected, &observed))
        }
        I22_CHECKER_ID => {
            let expected = decode_i22_input(input_bytes)?;
            let observed = decode_i22_observed(observed_bytes)?;
            if canonical_i22_input_bytes(&expected) != input_bytes
                || canonical_i22_observed_bytes(&observed) != observed_bytes
            {
                return Err("ingest-retention I22 retained bytes are not canonical".to_owned());
            }
            (I22_CHECKER_ID, first_i22_difference(&expected, &observed))
        }
        I23_CHECKER_ID => {
            let expected = decode_i23_input(input_bytes)?;
            let observed = decode_i23_observed(observed_bytes)?;
            if canonical_i23_input_bytes(&expected) != input_bytes
                || canonical_i23_observed_bytes(&observed) != observed_bytes
            {
                return Err("ingest-retention I23 retained bytes are not canonical".to_owned());
            }
            (I23_CHECKER_ID, first_i23_difference(&expected, &observed))
        }
        other => {
            return Err(format!(
                "unknown ingest-retention canonical checker {other}"
            ));
        }
    };
    Ok(IngestCanonicalReplay {
        checker_id,
        input_digest: canonical_digest(input_bytes),
        observed_digest: canonical_digest(observed_bytes),
        first_difference,
    })
}

struct CanonicalBytes {
    bytes: Vec<u8>,
}

impl CanonicalBytes {
    fn new(domain: &[u8]) -> Self {
        let mut canonical = Self { bytes: Vec::new() };
        canonical.blob(domain);
        canonical
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u128(&mut self, value: u128) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, value: usize) {
        self.u64(u64::try_from(value).unwrap_or(u64::MAX));
    }

    fn boolean(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn blob(&mut self, value: &[u8]) {
        self.len(value.len());
        self.bytes.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        self.blob(value.as_bytes());
    }

    fn document(&mut self, document: &DocumentFact) {
        self.u128(document.doc_id);
        self.u64(document.revision);
        self.len(document.timestamp_witness.len());
        for witness in &document.timestamp_witness {
            self.boolean(*witness);
        }
    }

    fn documents(&mut self, documents: &[DocumentFact]) {
        self.len(documents.len());
        for document in documents {
            self.document(document);
        }
    }

    fn ack(&mut self, ack: AckFact) {
        self.u64(ack.seq);
        self.u64(ack.generation);
    }

    fn optional_ack(&mut self, ack: Option<AckFact>) {
        self.boolean(ack.is_some());
        if let Some(ack) = ack {
            self.ack(ack);
        }
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        self.boolean(value.is_some());
        if let Some(value) = value {
            self.u64(value);
        }
    }

    fn optional_documents(&mut self, documents: Option<&[DocumentFact]>) {
        self.boolean(documents.is_some());
        if let Some(documents) = documents {
            self.documents(documents);
        }
    }

    fn strings(&mut self, values: &[String]) {
        self.len(values.len());
        for value in values {
            self.string(value);
        }
    }

    fn optional_strings(&mut self, values: Option<&[String]>) {
        self.boolean(values.is_some());
        if let Some(values) = values {
            self.strings(values);
        }
    }

    fn optional_string(&mut self, value: Option<&str>) {
        self.boolean(value.is_some());
        if let Some(value) = value {
            self.string(value);
        }
    }

    fn seal_result(&mut self, result: SealResultFact) {
        match result {
            SealResultFact::Committed { generation } => {
                self.u8(0);
                self.u64(generation);
            }
            SealResultFact::Cancelled => self.u8(1),
        }
    }

    fn range(&mut self, range: I22RangeFact) {
        self.i64(range.start);
        self.i64(range.end);
    }

    fn target_location(&mut self, location: I23TargetLocation) {
        self.u8(match location {
            I23TargetLocation::Active => 0,
            I23TargetLocation::Sealed => 1,
        });
    }

    fn delete_result(&mut self, result: I23DeleteResultFact) {
        match result {
            I23DeleteResultFact::Committed(ack) => {
                self.u8(0);
                self.ack(ack);
            }
            I23DeleteResultFact::Rejected => self.u8(1),
        }
    }

    fn sentinel_hits(&mut self, hits: &[I23SentinelHit]) {
        self.len(hits.len());
        for hit in hits {
            self.string(&hit.relative_path);
            self.u64(hit.offset);
            self.u64(hit.sentinel_index);
        }
    }

    fn i20_outcome(&mut self, outcome: &I20Outcome) {
        match outcome {
            I20Outcome::Commit => self.u8(0),
            I20Outcome::Reject { kind, detail } => {
                self.u8(1);
                self.string(kind);
                self.string(detail);
            }
        }
    }

    fn ingest_result(&mut self, result: &IngestResultFact) {
        match result {
            IngestResultFact::Committed(ack) => {
                self.u8(0);
                self.ack(*ack);
            }
            IngestResultFact::Rejected { kind, detail } => {
                self.u8(1);
                self.string(kind);
                self.string(detail);
            }
        }
    }

    fn optional_retry(&mut self, retry: Option<&RetryExpected>) {
        self.boolean(retry.is_some());
        if let Some(retry) = retry {
            self.ack(retry.ack);
            self.documents(&retry.live);
            self.u64(retry.wal_records_appended);
            self.u64(retry.generation_delta);
        }
    }
}

struct CanonicalReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> CanonicalReader<'a> {
    fn new(bytes: &'a [u8], domain: &[u8]) -> Result<Self, String> {
        let mut reader = Self { bytes, position: 0 };
        if reader.blob()? != domain {
            return Err("ingest-retention canonical domain mismatch".to_owned());
        }
        Ok(reader)
    }

    fn finish(self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "ingest-retention canonical bytes have {} trailing bytes",
                self.bytes.len().saturating_sub(self.position)
            ))
        }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| "ingest-retention canonical offset overflowed".to_owned())?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| "ingest-retention canonical bytes are truncated".to_owned())?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, String> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u64(&mut self) -> Result<u64, String> {
        self.take(8).map(|bytes| {
            u64::from_le_bytes(bytes.try_into().expect("canonical u64 width was checked"))
        })
    }

    fn i64(&mut self) -> Result<i64, String> {
        self.take(8).map(|bytes| {
            i64::from_le_bytes(bytes.try_into().expect("canonical i64 width was checked"))
        })
    }

    fn u128(&mut self) -> Result<u128, String> {
        self.take(16).map(|bytes| {
            u128::from_le_bytes(bytes.try_into().expect("canonical u128 width was checked"))
        })
    }

    fn len(&mut self, label: &str, maximum: usize) -> Result<usize, String> {
        let value = usize::try_from(self.u64()?)
            .map_err(|_| format!("ingest-retention canonical {label} length overflowed"))?;
        if value > maximum {
            return Err(format!(
                "ingest-retention canonical {label} length {value} exceeds {maximum}"
            ));
        }
        Ok(value)
    }

    fn boolean(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(format!(
                "ingest-retention canonical boolean tag {value} is invalid"
            )),
        }
    }

    fn blob(&mut self) -> Result<&'a [u8], String> {
        let length = self.len("blob", INGEST_CANONICAL_MAX_BYTES)?;
        self.take(length)
    }

    fn string(&mut self) -> Result<String, String> {
        std::str::from_utf8(self.blob()?)
            .map(str::to_owned)
            .map_err(|error| format!("ingest-retention canonical string is not UTF-8: {error}"))
    }

    fn document(&mut self) -> Result<DocumentFact, String> {
        let doc_id = self.u128()?;
        let revision = self.u64()?;
        let witnesses = self.len("timestamp witnesses", INGEST_CANONICAL_MAX_ITEMS)?;
        let mut timestamp_witness = Vec::with_capacity(witnesses);
        for _ in 0..witnesses {
            timestamp_witness.push(self.boolean()?);
        }
        Ok(DocumentFact {
            doc_id,
            revision,
            timestamp_witness,
        })
    }

    fn documents(&mut self) -> Result<Vec<DocumentFact>, String> {
        let count = self.len("documents", INGEST_CANONICAL_MAX_ITEMS)?;
        let mut documents = Vec::with_capacity(count);
        for _ in 0..count {
            documents.push(self.document()?);
        }
        Ok(documents)
    }

    fn ack(&mut self) -> Result<AckFact, String> {
        Ok(AckFact {
            seq: self.u64()?,
            generation: self.u64()?,
        })
    }

    fn optional_ack(&mut self) -> Result<Option<AckFact>, String> {
        self.boolean()?.then(|| self.ack()).transpose()
    }

    fn optional_u64(&mut self) -> Result<Option<u64>, String> {
        self.boolean()?.then(|| self.u64()).transpose()
    }

    fn optional_documents(&mut self) -> Result<Option<Vec<DocumentFact>>, String> {
        self.boolean()?.then(|| self.documents()).transpose()
    }

    fn strings(&mut self) -> Result<Vec<String>, String> {
        let count = self.len("strings", INGEST_CANONICAL_MAX_ITEMS)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.string()?);
        }
        Ok(values)
    }

    fn optional_strings(&mut self) -> Result<Option<Vec<String>>, String> {
        self.boolean()?.then(|| self.strings()).transpose()
    }

    fn optional_string(&mut self) -> Result<Option<String>, String> {
        self.boolean()?.then(|| self.string()).transpose()
    }

    fn seal_result(&mut self) -> Result<SealResultFact, String> {
        match self.u8()? {
            0 => Ok(SealResultFact::Committed {
                generation: self.u64()?,
            }),
            1 => Ok(SealResultFact::Cancelled),
            tag => Err(format!(
                "ingest-retention canonical seal result tag {tag} is invalid"
            )),
        }
    }

    fn range(&mut self) -> Result<I22RangeFact, String> {
        Ok(I22RangeFact {
            start: self.i64()?,
            end: self.i64()?,
        })
    }

    fn target_location(&mut self) -> Result<I23TargetLocation, String> {
        match self.u8()? {
            0 => Ok(I23TargetLocation::Active),
            1 => Ok(I23TargetLocation::Sealed),
            tag => Err(format!(
                "ingest-retention canonical target-location tag {tag} is invalid"
            )),
        }
    }

    fn delete_result(&mut self) -> Result<I23DeleteResultFact, String> {
        match self.u8()? {
            0 => Ok(I23DeleteResultFact::Committed(self.ack()?)),
            1 => Ok(I23DeleteResultFact::Rejected),
            tag => Err(format!(
                "ingest-retention canonical delete-result tag {tag} is invalid"
            )),
        }
    }

    fn sentinel_hits(&mut self) -> Result<Vec<I23SentinelHit>, String> {
        let count = self.len("sentinel hits", INGEST_CANONICAL_MAX_ITEMS)?;
        let mut hits = Vec::with_capacity(count);
        for _ in 0..count {
            hits.push(I23SentinelHit {
                relative_path: self.string()?,
                offset: self.u64()?,
                sentinel_index: self.u64()?,
            });
        }
        Ok(hits)
    }

    fn i20_outcome(&mut self) -> Result<I20Outcome, String> {
        match self.u8()? {
            0 => Ok(I20Outcome::Commit),
            1 => Ok(I20Outcome::Reject {
                kind: self.string()?,
                detail: self.string()?,
            }),
            tag => Err(format!(
                "ingest-retention canonical I20 outcome tag {tag} is invalid"
            )),
        }
    }

    fn ingest_result(&mut self) -> Result<IngestResultFact, String> {
        match self.u8()? {
            0 => Ok(IngestResultFact::Committed(self.ack()?)),
            1 => Ok(IngestResultFact::Rejected {
                kind: self.string()?,
                detail: self.string()?,
            }),
            tag => Err(format!(
                "ingest-retention canonical result tag {tag} is invalid"
            )),
        }
    }

    fn optional_retry(&mut self) -> Result<Option<RetryExpected>, String> {
        self.boolean()?
            .then(|| {
                Ok(RetryExpected {
                    ack: self.ack()?,
                    live: self.documents()?,
                    wal_records_appended: self.u64()?,
                    generation_delta: self.u64()?,
                })
            })
            .transpose()
    }
}

fn decode_i20_input(bytes: &[u8]) -> Result<I20Expected, String> {
    let mut reader = CanonicalReader::new(bytes, b"ingest-retention/I20/input/v1")?;
    let expected = I20Expected {
        baseline: reader.documents()?,
        submitted: reader.documents()?,
        outcome: reader.i20_outcome()?,
        final_live: reader.documents()?,
        expected_ack: reader.optional_ack()?,
        expected_generation_delta: reader.u64()?,
        expected_reopen_live: reader.documents()?,
        retry: reader.optional_retry()?,
    };
    reader.finish()?;
    Ok(expected)
}

fn decode_i20_observed(bytes: &[u8]) -> Result<I20Observed, String> {
    let mut reader = CanonicalReader::new(bytes, b"ingest-retention/I20/observed/v1")?;
    let observed = I20Observed {
        typed_result: reader.ingest_result()?,
        before_live: reader.documents()?,
        immediate_live: reader.documents()?,
        stats: I20StatsFact {
            active_rows: reader.u64()?,
            tombstones: reader.u64()?,
        },
        post_reopen_live: reader.documents()?,
        ack: reader.optional_ack()?,
        generation_before: reader.u64()?,
        generation_after: reader.u64()?,
        retry_ack: reader.optional_ack()?,
        retry_live: reader.optional_documents()?,
        retry_wal_records_appended: reader.optional_u64()?,
        retry_generation_delta: reader.optional_u64()?,
        receipt_digest: reader.optional_u64()?,
    };
    reader.finish()?;
    Ok(observed)
}

fn decode_i21_input(bytes: &[u8]) -> Result<I21Expected, String> {
    let mut reader = CanonicalReader::new(bytes, b"ingest-retention/I21/input/v1")?;
    let expected = I21Expected {
        live_before: reader.documents()?,
        live_after: reader.documents()?,
        active_rows_before: reader.u64()?,
        active_rows_after: reader.u64()?,
        generation_delta: reader.u64()?,
    };
    reader.finish()?;
    Ok(expected)
}

fn decode_i21_observed(bytes: &[u8]) -> Result<I21Observed, String> {
    let mut reader = CanonicalReader::new(bytes, b"ingest-retention/I21/observed/v1")?;
    let observed = I21Observed {
        seal_result: reader.seal_result()?,
        live_before: reader.documents()?,
        cancelled_live: reader.optional_documents()?,
        cancelled_active_rows: reader.optional_u64()?,
        cancelled_generation: reader.optional_u64()?,
        cancelled_orphan_paths: reader.optional_strings()?,
        retry_seal_generation: reader.optional_u64()?,
        live_after: reader.documents()?,
        live_after_reopen: reader.documents()?,
        active_rows_before: reader.u64()?,
        active_rows_after: reader.u64()?,
        generation_before: reader.u64()?,
        generation_after: reader.u64()?,
        orphan_paths: reader.strings()?,
        receipt_digest: reader.optional_u64()?,
    };
    reader.finish()?;
    Ok(observed)
}

fn decode_i22_input(bytes: &[u8]) -> Result<I22Expected, String> {
    let mut reader = CanonicalReader::new(bytes, b"ingest-retention/I22/input/v1")?;
    let now = reader.i64()?;
    let window = reader.i64()?;
    let cutoff = reader.i64()?;
    let drop_range = reader.range()?;
    let count = reader.len("partition bounds", INGEST_CANONICAL_MAX_ITEMS)?;
    let mut partition_bounds = Vec::with_capacity(count);
    for _ in 0..count {
        partition_bounds.push(I22PartitionBound {
            label: reader.string()?,
            min_ts: reader.i64()?,
            max_ts: reader.i64()?,
        });
    }
    let expected = I22Expected {
        now,
        window,
        cutoff,
        drop_range,
        partition_bounds,
        expected_dropped_labels: reader.strings()?,
        expected_straddler_labels: reader.strings()?,
        expected_retained_live: reader.documents()?,
        expected_active_control: reader.document()?,
    };
    reader.finish()?;
    Ok(expected)
}

fn decode_i22_observed(bytes: &[u8]) -> Result<I22Observed, String> {
    let mut reader = CanonicalReader::new(bytes, b"ingest-retention/I22/observed/v1")?;
    let observed = I22Observed {
        supplied_now: reader.i64()?,
        observed_cutoff: reader.i64()?,
        observed_drop_range: reader.range()?,
        generation_before: reader.u64()?,
        report_generation: reader.u64()?,
        manifest_committed: reader.boolean()?,
        dropped_labels: reader.strings()?,
        straddler_labels: reader.strings()?,
        bytes_reclaimed: reader.u64()?,
        live_after: reader.documents()?,
        live_after_reopen: reader.documents()?,
        active_control_present: reader.boolean()?,
        receipt_digest: reader.optional_u64()?,
    };
    reader.finish()?;
    Ok(observed)
}

fn decode_i23_input(bytes: &[u8]) -> Result<I23Expected, String> {
    let mut reader = CanonicalReader::new(bytes, b"ingest-retention/I23/input/v1")?;
    let target = reader.document()?;
    let target_location = reader.target_location()?;
    let count = reader.len("sentinel patterns", INGEST_CANONICAL_MAX_ITEMS)?;
    let mut sentinel_patterns = Vec::with_capacity(count);
    for _ in 0..count {
        sentinel_patterns.push(I23SentinelPattern {
            index: reader.u64()?,
            bytes: reader.blob()?.to_vec(),
        });
    }
    let expected = I23Expected {
        target,
        target_location,
        sentinel_patterns,
        expected_logically_live: reader.documents()?,
        expected_physically_live: reader.documents()?,
        token_no_op: reader.boolean()?,
        intent_after_completion: reader.boolean()?,
    };
    reader.finish()?;
    Ok(expected)
}

fn decode_i23_observed(bytes: &[u8]) -> Result<I23Observed, String> {
    let mut reader = CanonicalReader::new(bytes, b"ingest-retention/I23/observed/v1")?;
    let delete_result = reader.delete_result()?;
    let logical_search = reader.documents()?;
    let pre_purge_hits = reader.sentinel_hits()?;
    let purge_token = I23PurgeTokenFact {
        token_id: reader.u64()?,
        no_op: reader.boolean()?,
    };
    let fault_error = reader.optional_string()?;
    let await_result = I23AwaitResultFact {
        completed: reader.boolean()?,
        generation: reader.u64()?,
    };
    let post_purge_hits = reader.sentinel_hits()?;
    let intent_present = reader.boolean()?;
    let immediate_live = reader.documents()?;
    let reopen_live = reader.documents()?;
    let second_reopen_live = reader.documents()?;
    let generation_facts = I23GenerationFacts {
        before_delete: reader.u64()?,
        after_delete: reader.u64()?,
        after_purge: reader.u64()?,
        after_reopen: reader.u64()?,
        after_second_reopen: reader.u64()?,
    };
    let receipt_digest = reader.optional_u64()?;
    reader.finish()?;
    Ok(I23Observed {
        delete_result,
        logical_search,
        pre_purge_hits,
        purge_token,
        fault_error,
        await_result,
        post_purge_hits,
        intent_present,
        immediate_live,
        reopen_live,
        second_reopen_live,
        generation_facts,
        receipt_digest,
    })
}

pub fn expected_i23(
    initial_live: &[DocumentFact],
    target_doc_id: u128,
    target_location: I23TargetLocation,
    sentinel_patterns: Vec<I23SentinelPattern>,
) -> Result<I23Expected, String> {
    if sentinel_patterns.is_empty()
        || sentinel_patterns
            .iter()
            .any(|pattern| pattern.bytes.is_empty())
    {
        return Err(format!("{I23_CHECKER_ID} sentinel catalog is empty"));
    }
    let mut live = initial_live
        .iter()
        .cloned()
        .map(|document| (document.doc_id, document))
        .collect::<BTreeMap<_, _>>();
    let target = live.remove(&target_doc_id).ok_or_else(|| {
        format!("{I23_CHECKER_ID} target {target_doc_id} is absent from primitive live state")
    })?;
    let survivors = live.into_values().collect::<Vec<_>>();
    Ok(I23Expected {
        target,
        target_location,
        sentinel_patterns,
        expected_logically_live: survivors.clone(),
        expected_physically_live: survivors,
        token_no_op: false,
        intent_after_completion: false,
    })
}

pub fn compare_i23(expected: &I23Expected, observed: &I23Observed) -> Result<(), String> {
    if !matches!(observed.delete_result, I23DeleteResultFact::Committed(_)) {
        return Err(format!("{I23_CHECKER_ID} logical delete did not commit"));
    }
    compare_i23_documents(
        "logical_search",
        &expected.expected_logically_live,
        &observed.logical_search,
    )?;
    for pattern in &expected.sentinel_patterns {
        if !observed
            .pre_purge_hits
            .iter()
            .any(|hit| hit.sentinel_index == pattern.index)
        {
            return Err(format!(
                "{I23_CHECKER_ID} sentinel {} was not proved present before purge",
                pattern.index
            ));
        }
    }
    if observed.purge_token.no_op != expected.token_no_op || observed.purge_token.token_id == 0 {
        return Err(format!(
            "{I23_CHECKER_ID} purge token mismatch expected no_op={} observed={:?}",
            expected.token_no_op, observed.purge_token
        ));
    }
    if !observed.await_result.completed {
        return Err(format!(
            "{I23_CHECKER_ID} physical purge did not report completion"
        ));
    }
    if let Some(hit) = observed.post_purge_hits.first() {
        return Err(format!(
            "{I23_CHECKER_ID} completed purge left sentinel in {}@{}",
            hit.relative_path, hit.offset
        ));
    }
    if observed.intent_present != expected.intent_after_completion {
        return Err(format!(
            "{I23_CHECKER_ID} purge intent presence mismatch expected={} observed={}",
            expected.intent_after_completion, observed.intent_present
        ));
    }
    if observed
        .immediate_live
        .iter()
        .chain(&observed.reopen_live)
        .chain(&observed.second_reopen_live)
        .any(|document| document.doc_id == expected.target.doc_id)
    {
        return Err(format!(
            "{I23_CHECKER_ID} target {} resurrected after completed purge",
            expected.target.doc_id
        ));
    }
    compare_i23_documents(
        "immediate_live",
        &expected.expected_physically_live,
        &observed.immediate_live,
    )?;
    compare_i23_documents(
        "reopen_live",
        &expected.expected_physically_live,
        &observed.reopen_live,
    )?;
    compare_i23_documents(
        "second_reopen_live",
        &expected.expected_physically_live,
        &observed.second_reopen_live,
    )?;
    let generations = observed.generation_facts;
    if generations.after_delete < generations.before_delete
        || generations.after_purge < generations.after_delete
        || generations.after_reopen < generations.after_purge
        || generations.after_second_reopen < generations.after_reopen
        || observed.await_result.generation != generations.after_purge
    {
        return Err(format!(
            "{I23_CHECKER_ID} generation facts regressed or disagreed with await result: {generations:?} await={:?}",
            observed.await_result
        ));
    }
    Ok(())
}

fn compare_i23_documents(
    field: &str,
    expected: &[DocumentFact],
    observed: &[DocumentFact],
) -> Result<(), String> {
    let mut expected = expected.to_vec();
    expected.sort();
    let mut observed = observed.to_vec();
    observed.sort();
    if expected == observed {
        return Ok(());
    }
    Err(format!(
        "{I23_CHECKER_ID} {field} mismatch expected={expected:?} observed={observed:?}"
    ))
}

pub fn i23_completed_sentinel_hit_plant_error() -> Result<String, String> {
    let target = DocumentFact {
        doc_id: 41,
        revision: 3,
        timestamp_witness: vec![true, true],
    };
    let survivor = DocumentFact {
        doc_id: 42,
        revision: 1,
        timestamp_witness: vec![false, true],
    };
    let expected = expected_i23(
        &[target, survivor.clone()],
        41,
        I23TargetLocation::Sealed,
        vec![I23SentinelPattern {
            index: 0,
            bytes: b"purge-sentinel-41".to_vec(),
        }],
    )?;
    let hit = I23SentinelHit {
        relative_path: "segment-0001.zseg".to_owned(),
        offset: 128,
        sentinel_index: 0,
    };
    let observed = I23Observed {
        delete_result: I23DeleteResultFact::Committed(AckFact {
            seq: 7,
            generation: 5,
        }),
        logical_search: vec![survivor.clone()],
        pre_purge_hits: vec![hit.clone()],
        purge_token: I23PurgeTokenFact {
            token_id: 9,
            no_op: false,
        },
        fault_error: None,
        await_result: I23AwaitResultFact {
            completed: true,
            generation: 6,
        },
        post_purge_hits: vec![hit],
        intent_present: false,
        immediate_live: vec![survivor.clone()],
        reopen_live: vec![survivor.clone()],
        second_reopen_live: vec![survivor],
        generation_facts: I23GenerationFacts {
            before_delete: 4,
            after_delete: 5,
            after_purge: 6,
            after_reopen: 6,
            after_second_reopen: 6,
        },
        receipt_digest: None,
    };
    compare_i23(&expected, &observed).map_or_else(Ok, |()| {
        Err(format!(
            "{I23_CHECKER_ID} completed purge left sentinel in segment-0001.zseg@128 but the malformed observation was accepted"
        ))
    })
}

pub fn i23_resurrection_plant_error() -> Result<String, String> {
    let target = DocumentFact {
        doc_id: 51,
        revision: 2,
        timestamp_witness: vec![true],
    };
    let survivor = DocumentFact {
        doc_id: 52,
        revision: 1,
        timestamp_witness: vec![true],
    };
    let expected = expected_i23(
        &[target.clone(), survivor.clone()],
        target.doc_id,
        I23TargetLocation::Active,
        vec![I23SentinelPattern {
            index: 0,
            bytes: b"purge-sentinel-51".to_vec(),
        }],
    )?;
    let observed = I23Observed {
        delete_result: I23DeleteResultFact::Committed(AckFact {
            seq: 3,
            generation: 3,
        }),
        logical_search: vec![survivor.clone()],
        pre_purge_hits: vec![I23SentinelHit {
            relative_path: "wal.ze".to_owned(),
            offset: 72,
            sentinel_index: 0,
        }],
        purge_token: I23PurgeTokenFact {
            token_id: 4,
            no_op: false,
        },
        fault_error: None,
        await_result: I23AwaitResultFact {
            completed: true,
            generation: 4,
        },
        post_purge_hits: Vec::new(),
        intent_present: false,
        immediate_live: vec![survivor.clone()],
        reopen_live: vec![survivor.clone(), target],
        second_reopen_live: vec![survivor],
        generation_facts: I23GenerationFacts {
            before_delete: 2,
            after_delete: 3,
            after_purge: 4,
            after_reopen: 4,
            after_second_reopen: 4,
        },
        receipt_digest: None,
    };
    compare_i23(&expected, &observed).map_or_else(Ok, |()| {
        Err(format!(
            "{I23_CHECKER_ID} target {} resurrection was accepted",
            expected.target.doc_id
        ))
    })
}

pub fn expected_i22(
    now: i64,
    window: i64,
    partitions: &[I22PartitionInput],
    active_control: DocumentFact,
) -> Result<I22Expected, String> {
    if window <= 0 {
        return Err(format!(
            "{I22_CHECKER_ID} retention window {window} must be positive"
        ));
    }
    let cutoff = now.saturating_sub(window);
    let mut partition_bounds = Vec::with_capacity(partitions.len());
    let mut expected_dropped_labels = Vec::new();
    let mut expected_straddler_labels = Vec::new();
    let mut expected_retained_live = Vec::new();
    for partition in partitions {
        let min_ts = partition
            .rows
            .iter()
            .map(|(timestamp, _)| *timestamp)
            .min()
            .ok_or_else(|| format!("{I22_CHECKER_ID} partition {} is empty", partition.label))?;
        let max_ts = partition
            .rows
            .iter()
            .map(|(timestamp, _)| *timestamp)
            .max()
            .ok_or_else(|| format!("{I22_CHECKER_ID} partition {} is empty", partition.label))?;
        partition_bounds.push(I22PartitionBound {
            label: partition.label.clone(),
            min_ts,
            max_ts,
        });
        if partition
            .rows
            .iter()
            .all(|(timestamp, _)| *timestamp < cutoff)
        {
            expected_dropped_labels.push(partition.label.clone());
        } else {
            if partition
                .rows
                .iter()
                .any(|(timestamp, _)| *timestamp < cutoff)
                && partition
                    .rows
                    .iter()
                    .any(|(timestamp, _)| *timestamp >= cutoff)
            {
                expected_straddler_labels.push(partition.label.clone());
            }
            expected_retained_live
                .extend(partition.rows.iter().map(|(_, document)| document.clone()));
        }
    }
    expected_retained_live.push(active_control.clone());
    expected_retained_live.sort();
    Ok(I22Expected {
        now,
        window,
        cutoff,
        drop_range: I22RangeFact {
            start: i64::MIN,
            end: cutoff,
        },
        partition_bounds,
        expected_dropped_labels,
        expected_straddler_labels,
        expected_retained_live,
        expected_active_control: active_control,
    })
}

pub fn compare_i22(expected: &I22Expected, observed: &I22Observed) -> Result<(), String> {
    if observed.supplied_now != expected.now {
        return Err(format!(
            "{I22_CHECKER_ID} supplied now mismatch expected={} observed={}",
            expected.now, observed.supplied_now
        ));
    }
    if observed.observed_cutoff != expected.cutoff {
        return Err(format!(
            "{I22_CHECKER_ID} cutoff mismatch expected={} observed={}",
            expected.cutoff, observed.observed_cutoff
        ));
    }
    if observed.observed_drop_range != expected.drop_range {
        return Err(format!(
            "{I22_CHECKER_ID} drop range mismatch expected={:?} observed={:?}",
            expected.drop_range, observed.observed_drop_range
        ));
    }
    if observed.dropped_labels.iter().any(|label| {
        expected
            .partition_bounds
            .iter()
            .any(|partition| partition.label == *label && partition.max_ts >= expected.cutoff)
    }) {
        return Err(format!("{I22_CHECKER_ID} cutoff row was dropped"));
    }
    if observed.dropped_labels != expected.expected_dropped_labels {
        return Err(format!(
            "{I22_CHECKER_ID} dropped partitions mismatch expected={:?} observed={:?}",
            expected.expected_dropped_labels, observed.dropped_labels
        ));
    }
    if observed.straddler_labels != expected.expected_straddler_labels {
        return Err(format!(
            "{I22_CHECKER_ID} straddlers mismatch expected={:?} observed={:?}",
            expected.expected_straddler_labels, observed.straddler_labels
        ));
    }
    let expected_commit = !expected.expected_dropped_labels.is_empty();
    if observed.manifest_committed != expected_commit {
        return Err(format!(
            "{I22_CHECKER_ID} manifest commit mismatch expected={expected_commit} observed={}",
            observed.manifest_committed
        ));
    }
    let expected_generation = observed
        .generation_before
        .checked_add(u64::from(expected_commit))
        .ok_or_else(|| format!("{I22_CHECKER_ID} expected generation overflows"))?;
    if observed.report_generation != expected_generation {
        return Err(format!(
            "{I22_CHECKER_ID} report generation mismatch expected={expected_generation} observed={}",
            observed.report_generation
        ));
    }
    if expected_commit && observed.bytes_reclaimed == 0 {
        return Err(format!(
            "{I22_CHECKER_ID} committed retention reported zero reclaimed bytes"
        ));
    }
    compare_i22_documents(
        "live_after",
        &expected.expected_retained_live,
        &observed.live_after,
    )?;
    compare_i22_documents(
        "live_after_reopen",
        &expected.expected_retained_live,
        &observed.live_after_reopen,
    )?;
    if !observed.active_control_present
        || !observed
            .live_after
            .contains(&expected.expected_active_control)
        || !observed
            .live_after_reopen
            .contains(&expected.expected_active_control)
    {
        return Err(format!(
            "{I22_CHECKER_ID} active control was selected by whole-segment retention"
        ));
    }
    Ok(())
}

fn compare_i22_documents(
    field: &str,
    expected: &[DocumentFact],
    observed: &[DocumentFact],
) -> Result<(), String> {
    let mut expected = expected.to_vec();
    expected.sort();
    let mut observed = observed.to_vec();
    observed.sort();
    if expected == observed {
        return Ok(());
    }
    Err(format!(
        "{I22_CHECKER_ID} {field} mismatch expected={expected:?} observed={observed:?}"
    ))
}

pub fn i22_inclusive_cutoff_drop_plant_error() -> Result<String, String> {
    let document = |doc_id, witnesses| DocumentFact {
        doc_id,
        revision: 1,
        timestamp_witness: witnesses,
    };
    let at = document(22, vec![false, true, true]);
    let after = document(23, vec![false, false, true]);
    let straddler_before = document(24, vec![true, true, true]);
    let straddler_at = document(25, vec![false, true, true]);
    let active = document(26, vec![false, false, true]);
    let expected = expected_i22(
        20,
        10,
        &[
            I22PartitionInput {
                label: "before".to_owned(),
                rows: vec![(9, document(21, vec![true, true, true]))],
            },
            I22PartitionInput {
                label: "at".to_owned(),
                rows: vec![(10, at.clone())],
            },
            I22PartitionInput {
                label: "after".to_owned(),
                rows: vec![(11, after.clone())],
            },
            I22PartitionInput {
                label: "straddler".to_owned(),
                rows: vec![(9, straddler_before.clone()), (10, straddler_at.clone())],
            },
        ],
        active,
    )?;
    let observed = I22Observed {
        supplied_now: 20,
        observed_cutoff: 10,
        observed_drop_range: I22RangeFact {
            start: i64::MIN,
            end: 10,
        },
        generation_before: 4,
        report_generation: 5,
        manifest_committed: true,
        dropped_labels: vec!["before".to_owned(), "at".to_owned()],
        straddler_labels: vec!["straddler".to_owned()],
        bytes_reclaimed: 4096,
        live_after: expected.expected_retained_live.clone(),
        live_after_reopen: expected.expected_retained_live.clone(),
        active_control_present: true,
        receipt_digest: None,
    };
    compare_i22(&expected, &observed).map_or_else(Ok, |()| {
        Err(format!(
            "{I22_CHECKER_ID} cutoff row was dropped but the malformed observation was accepted"
        ))
    })
}

pub fn i22_cutoff_shift_plant_error() -> Result<String, String> {
    let document = |doc_id, witnesses| DocumentFact {
        doc_id,
        revision: 1,
        timestamp_witness: witnesses,
    };
    let at = document(32, vec![false, true, true]);
    let active = document(33, vec![false, false, true]);
    let expected = expected_i22(
        20,
        10,
        &[
            I22PartitionInput {
                label: "before".to_owned(),
                rows: vec![(9, document(31, vec![true, true, true]))],
            },
            I22PartitionInput {
                label: "at".to_owned(),
                rows: vec![(10, at)],
            },
        ],
        active,
    )?;
    let observed = I22Observed {
        supplied_now: 20,
        observed_cutoff: 11,
        observed_drop_range: I22RangeFact {
            start: i64::MIN,
            end: 11,
        },
        generation_before: 4,
        report_generation: 5,
        manifest_committed: true,
        dropped_labels: expected.expected_dropped_labels.clone(),
        straddler_labels: expected.expected_straddler_labels.clone(),
        bytes_reclaimed: 4096,
        live_after: expected.expected_retained_live.clone(),
        live_after_reopen: expected.expected_retained_live.clone(),
        active_control_present: true,
        receipt_digest: None,
    };
    compare_i22(&expected, &observed).map_or_else(Ok, |()| {
        Err(format!("{I22_CHECKER_ID} cutoff mismatch was accepted"))
    })
}

pub fn expected_i21(
    initial: &[DocumentFact],
    upserts: &[DocumentFact],
    deleted_doc_ids: &[u128],
    active_rows_before: u64,
) -> I21Expected {
    let mut live = initial
        .iter()
        .cloned()
        .map(|document| (document.doc_id, document))
        .collect::<BTreeMap<_, _>>();
    for document in upserts {
        live.insert(document.doc_id, document.clone());
    }
    for doc_id in deleted_doc_ids {
        live.remove(doc_id);
    }
    let live = live.into_values().collect::<Vec<_>>();
    I21Expected {
        live_before: live.clone(),
        live_after: live,
        active_rows_before,
        active_rows_after: 0,
        generation_delta: 1,
    }
}

pub fn compare_i21(expected: &I21Expected, observed: &I21Observed) -> Result<(), String> {
    compare_i21_documents("live_before", &expected.live_before, &observed.live_before)?;
    match observed.seal_result {
        SealResultFact::Committed { generation } => {
            if generation != observed.generation_after {
                return Err(format!(
                    "{I21_CHECKER_ID} committed seal generation {generation} differs from observed generation {}",
                    observed.generation_after
                ));
            }
            if observed.cancelled_live.is_some()
                || observed.cancelled_active_rows.is_some()
                || observed.cancelled_generation.is_some()
                || observed.cancelled_orphan_paths.is_some()
                || observed.retry_seal_generation.is_some()
                || observed.receipt_digest.is_some()
            {
                return Err(format!(
                    "{I21_CHECKER_ID} clean seal carried cancellation or receipt evidence"
                ));
            }
        }
        SealResultFact::Cancelled => {
            let cancelled_live = observed.cancelled_live.as_ref().ok_or_else(|| {
                format!("{I21_CHECKER_ID} cancelled seal omitted its live multiset")
            })?;
            compare_i21_documents("cancelled_live", &expected.live_before, cancelled_live)?;
            if observed.cancelled_active_rows != Some(expected.active_rows_before)
                || observed.cancelled_generation != Some(observed.generation_before)
            {
                return Err(format!(
                    "{I21_CHECKER_ID} cancelled seal mutated active rows/generation expected={}/{} observed={:?}/{:?}",
                    expected.active_rows_before,
                    observed.generation_before,
                    observed.cancelled_active_rows,
                    observed.cancelled_generation
                ));
            }
            if observed
                .cancelled_orphan_paths
                .as_ref()
                .is_none_or(|paths| !paths.is_empty())
            {
                return Err(format!(
                    "{I21_CHECKER_ID} cancelled seal omitted cleanup proof or left orphan paths {:?}",
                    observed.cancelled_orphan_paths
                ));
            }
            if observed.retry_seal_generation != Some(observed.generation_after)
                || observed.receipt_digest.is_none()
            {
                return Err(format!(
                    "{I21_CHECKER_ID} cancelled seal omitted retry generation or product receipt"
                ));
            }
        }
    }
    compare_i21_documents("live_after", &expected.live_after, &observed.live_after)?;
    compare_i21_documents(
        "live_after_reopen",
        &expected.live_after,
        &observed.live_after_reopen,
    )?;
    if observed.active_rows_before != expected.active_rows_before
        || observed.active_rows_after != expected.active_rows_after
    {
        return Err(format!(
            "{I21_CHECKER_ID} active row counts mismatch expected={}/{} observed={}/{}",
            expected.active_rows_before,
            expected.active_rows_after,
            observed.active_rows_before,
            observed.active_rows_after
        ));
    }
    let generation_delta = observed
        .generation_after
        .checked_sub(observed.generation_before)
        .ok_or_else(|| {
            format!(
                "{I21_CHECKER_ID} generation regressed before={} after={}",
                observed.generation_before, observed.generation_after
            )
        })?;
    if generation_delta != expected.generation_delta {
        return Err(format!(
            "{I21_CHECKER_ID} generation delta mismatch expected={} observed={generation_delta}",
            expected.generation_delta
        ));
    }
    if !observed.orphan_paths.is_empty() {
        return Err(format!(
            "{I21_CHECKER_ID} seal left orphan paths {:?}",
            observed.orphan_paths
        ));
    }
    Ok(())
}

fn compare_i21_documents(
    field: &str,
    expected: &[DocumentFact],
    observed: &[DocumentFact],
) -> Result<(), String> {
    let mut unmatched = observed.to_vec();
    for document in expected {
        if let Some(position) = unmatched.iter().position(|candidate| candidate == document) {
            unmatched.remove(position);
        } else {
            return Err(format!(
                "{I21_CHECKER_ID} missing document {}@{} from {field}",
                document.doc_id, document.revision
            ));
        }
    }
    if !unmatched.is_empty() {
        return Err(format!(
            "{I21_CHECKER_ID} {field} contains unexpected documents {unmatched:?}"
        ));
    }
    Ok(())
}

pub fn i21_missing_document_plant_error() -> Result<String, String> {
    let live = vec![
        DocumentFact {
            doc_id: 21,
            revision: 2,
            timestamp_witness: vec![true, true],
        },
        DocumentFact {
            doc_id: 22,
            revision: 1,
            timestamp_witness: vec![false, true],
        },
    ];
    let expected = expected_i21(&live, &[], &[], 2);
    let observed = I21Observed {
        seal_result: SealResultFact::Committed { generation: 8 },
        live_before: live.clone(),
        cancelled_live: None,
        cancelled_active_rows: None,
        cancelled_generation: None,
        cancelled_orphan_paths: None,
        retry_seal_generation: None,
        live_after: vec![live[1].clone()],
        live_after_reopen: live,
        active_rows_before: 2,
        active_rows_after: 0,
        generation_before: 7,
        generation_after: 8,
        orphan_paths: Vec::new(),
        receipt_digest: None,
    };
    compare_i21(&expected, &observed).map_or_else(Ok, |()| {
        Err("I21.seal-multiset.v1 missing document 21@2 was accepted".to_owned())
    })
}

pub fn expected_i20(
    baseline: &[DocumentFact],
    submitted: &[DocumentFact],
    outcome: I20Outcome,
    expected_ack: Option<AckFact>,
    expected_generation_delta: u64,
    retry: Option<RetryExpected>,
) -> Result<I20Expected, String> {
    let mut live = baseline
        .iter()
        .cloned()
        .map(|document| (document.doc_id, document))
        .collect::<BTreeMap<_, _>>();
    if matches!(outcome, I20Outcome::Commit) {
        for document in submitted {
            live.insert(document.doc_id, document.clone());
        }
    }
    let final_live = live.into_values().collect::<Vec<_>>();
    Ok(I20Expected {
        baseline: sorted(baseline),
        submitted: submitted.to_vec(),
        outcome,
        final_live: final_live.clone(),
        expected_ack,
        expected_generation_delta,
        expected_reopen_live: final_live,
        retry,
    })
}

/// Primitive byte lengths needed to independently size one persisted upsert-v2
/// WAL record without importing the production encoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpsertV2Shape {
    pub vector_dimensions: u64,
    pub text_bytes: Option<u64>,
    pub timestamp_present: bool,
    pub metadata_bytes: Option<u64>,
    pub typed_columns_encoded_bytes: Option<u64>,
}

/// Returns the exact encoded WAL group length for the supplied upsert-v2
/// shapes using the frozen record and payload layouts.
pub fn encoded_upsert_v2_group_bytes(shapes: &[UpsertV2Shape]) -> Result<u64, String> {
    const RECORD_HEADER_BYTES: u64 = 14;
    const RECORD_CHECKSUM_BYTES: u64 = 8;
    const FIELD_BITMAP_BYTES: u64 = 4;
    const DOCUMENT_VERSION_BYTES: u64 = 24;
    const VECTOR_LENGTH_BYTES: u64 = 4;
    const LENGTH_PREFIX_BYTES: u64 = 4;
    const TIMESTAMP_BYTES: u64 = 8;

    shapes.iter().try_fold(0_u64, |group, shape| {
        let vector_bytes = shape
            .vector_dimensions
            .checked_mul(4)
            .ok_or_else(|| "I20 vector byte length overflows u64".to_owned())?;
        let mut payload = FIELD_BITMAP_BYTES
            .checked_add(DOCUMENT_VERSION_BYTES)
            .and_then(|value| value.checked_add(VECTOR_LENGTH_BYTES))
            .and_then(|value| value.checked_add(vector_bytes))
            .ok_or_else(|| "I20 upsert-v2 fixed payload length overflows u64".to_owned())?;
        if let Some(text_bytes) = shape.text_bytes {
            payload = payload
                .checked_add(LENGTH_PREFIX_BYTES)
                .and_then(|value| value.checked_add(text_bytes))
                .ok_or_else(|| "I20 upsert-v2 text length overflows u64".to_owned())?;
        }
        if shape.timestamp_present {
            payload = payload
                .checked_add(TIMESTAMP_BYTES)
                .ok_or_else(|| "I20 upsert-v2 timestamp length overflows u64".to_owned())?;
        }
        if let Some(metadata_bytes) = shape.metadata_bytes {
            payload = payload
                .checked_add(LENGTH_PREFIX_BYTES)
                .and_then(|value| value.checked_add(metadata_bytes))
                .ok_or_else(|| "I20 upsert-v2 metadata length overflows u64".to_owned())?;
        }
        if let Some(columns_bytes) = shape.typed_columns_encoded_bytes {
            payload = payload
                .checked_add(columns_bytes)
                .ok_or_else(|| "I20 upsert-v2 column length overflows u64".to_owned())?;
        }
        let record = RECORD_HEADER_BYTES
            .checked_add(payload)
            .and_then(|value| value.checked_add(RECORD_CHECKSUM_BYTES))
            .ok_or_else(|| "I20 WAL record length overflows u64".to_owned())?;
        group
            .checked_add(record)
            .ok_or_else(|| "I20 WAL group length overflows u64".to_owned())
    })
}

/// Compares an I20 public observation with its independently derived model.
///
/// The explicit rejected-subset guard is added in the RED-to-GREEN slice; all
/// remaining exact relations are already expressed here so that the plant is
/// isolated to one contract.
pub fn compare_i20(expected: &I20Expected, observed: &I20Observed) -> Result<(), String> {
    compare_documents("before_live", &expected.baseline, &observed.before_live)?;

    match (&expected.outcome, &observed.typed_result) {
        (I20Outcome::Commit, IngestResultFact::Committed(ack)) => {
            if Some(*ack) != expected.expected_ack || observed.ack != expected.expected_ack {
                return Err(format!(
                    "{I20_CHECKER_ID} acknowledgement mismatch expected={:?} result={ack:?} observed={:?}",
                    expected.expected_ack, observed.ack
                ));
            }
        }
        (
            I20Outcome::Reject {
                kind: expected_kind,
                detail: expected_detail,
            },
            IngestResultFact::Rejected {
                kind: observed_kind,
                detail: observed_detail,
            },
        ) if expected_kind == observed_kind && expected_detail == observed_detail => {
            if observed.ack.is_some() {
                return Err(format!(
                    "{I20_CHECKER_ID} rejected batch carried an acknowledgement"
                ));
            }
        }
        (expected_outcome, actual) => {
            return Err(format!(
                "{I20_CHECKER_ID} typed result mismatch expected={expected_outcome:?} observed={actual:?}"
            ));
        }
    }

    let observed_generation_delta = observed
        .generation_after
        .checked_sub(observed.generation_before)
        .ok_or_else(|| {
            format!(
                "{I20_CHECKER_ID} generation regressed before={} after={}",
                observed.generation_before, observed.generation_after
            )
        })?;
    if observed_generation_delta != expected.expected_generation_delta {
        return Err(format!(
            "{I20_CHECKER_ID} generation delta mismatch expected={} observed={observed_generation_delta}",
            expected.expected_generation_delta
        ));
    }

    if matches!(expected.outcome, I20Outcome::Reject { .. }) {
        for (field, live) in [
            ("immediate_live", observed.immediate_live.as_slice()),
            ("post_reopen_live", observed.post_reopen_live.as_slice()),
        ] {
            let visible = expected
                .submitted
                .iter()
                .filter(|submitted| live.contains(submitted))
                .count();
            if visible > 0 && visible < expected.submitted.len() {
                return Err(format!(
                    "{I20_CHECKER_ID} visible subset {visible}/{} in {field} after rejected batch",
                    expected.submitted.len()
                ));
            }
        }
    }

    compare_documents(
        "immediate_live",
        &expected.final_live,
        &observed.immediate_live,
    )?;
    compare_documents(
        "post_reopen_live",
        &expected.expected_reopen_live,
        &observed.post_reopen_live,
    )?;

    match (&expected.retry, observed.retry_ack, &observed.retry_live) {
        (None, None, None) => {}
        (Some(retry), Some(ack), Some(live)) => {
            if ack != retry.ack {
                return Err(format!(
                    "{I20_CHECKER_ID} retry acknowledgement mismatch expected={:?} observed={ack:?}",
                    retry.ack
                ));
            }
            compare_documents("retry_live", &retry.live, live)?;
            if observed.retry_wal_records_appended != Some(retry.wal_records_appended)
                || observed.retry_generation_delta != Some(retry.generation_delta)
            {
                return Err(format!(
                    "{I20_CHECKER_ID} retry mutated WAL/generation expected=({},{}) observed=({:?},{:?})",
                    retry.wal_records_appended,
                    retry.generation_delta,
                    observed.retry_wal_records_appended,
                    observed.retry_generation_delta
                ));
            }
        }
        _ => return Err(format!("{I20_CHECKER_ID} retry evidence is incomplete")),
    }
    Ok(())
}

fn sorted(documents: &[DocumentFact]) -> Vec<DocumentFact> {
    let mut sorted = documents.to_vec();
    sorted.sort();
    sorted
}

fn compare_documents(
    field: &str,
    expected: &[DocumentFact],
    observed: &[DocumentFact],
) -> Result<(), String> {
    let expected = sorted(expected);
    let observed = sorted(observed);
    if expected == observed {
        return Ok(());
    }
    Err(format!(
        "{I20_CHECKER_ID} {field} mismatch expected={expected:?} observed={observed:?}"
    ))
}

/// Runs the canonical rejected-batch subset plant and returns its exact
/// comparator refusal. Shared tests call this without importing production.
pub fn i20_visible_subset_plant_error() -> Result<String, String> {
    let baseline = vec![DocumentFact {
        doc_id: 1,
        revision: 1,
        timestamp_witness: vec![true, true],
    }];
    let submitted = vec![
        DocumentFact {
            doc_id: 2,
            revision: 1,
            timestamp_witness: vec![false, true],
        },
        DocumentFact {
            doc_id: 3,
            revision: 1,
            timestamp_witness: vec![false, false],
        },
    ];
    let expected = expected_i20(
        &baseline,
        &submitted,
        I20Outcome::Reject {
            kind: "wal-write".to_owned(),
            detail: "injected partial-batch-append after 19/97 bytes".to_owned(),
        },
        None,
        0,
        None,
    )?;
    let mut immediate_live = baseline.clone();
    immediate_live.push(submitted[0].clone());
    let observed = I20Observed {
        typed_result: IngestResultFact::Rejected {
            kind: "wal-write".to_owned(),
            detail: "injected partial-batch-append after 19/97 bytes".to_owned(),
        },
        before_live: baseline,
        immediate_live: immediate_live.clone(),
        stats: I20StatsFact {
            active_rows: 2,
            tombstones: 0,
        },
        post_reopen_live: immediate_live,
        ack: None,
        generation_before: 7,
        generation_after: 7,
        retry_ack: None,
        retry_live: None,
        retry_wal_records_appended: None,
        retry_generation_delta: None,
        receipt_digest: None,
    };
    compare_i20(&expected, &observed).map_or_else(Ok, |()| {
        Err("I20.batch-atomicity.v1 visible subset 1/2 was accepted".to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i20_batch_atomicity_checker_rejects_one_visible_row_from_failed_batch() {
        let error = i20_visible_subset_plant_error().expect("valid I20 plant");
        assert!(
            error.contains("I20.batch-atomicity.v1 visible subset 1/2"),
            "{error}"
        );
    }

    #[test]
    fn i21_seal_checker_rejects_a_missing_document() {
        let error = i21_missing_document_plant_error().expect("valid I21 plant");
        assert!(
            error.contains("I21.seal-multiset.v1 missing document 21@2"),
            "{error}"
        );
    }

    #[test]
    fn i22_retention_checker_rejects_inclusive_cutoff_drop() {
        let error = i22_inclusive_cutoff_drop_plant_error().expect("valid I22 plant");
        assert!(
            error.contains("I22.retention-boundary.v1 cutoff row was dropped"),
            "{error}"
        );
    }

    #[test]
    fn i22_retention_checker_rejects_a_one_tick_cutoff_shift() {
        let error = i22_cutoff_shift_plant_error().expect("valid I22 cutoff-shift plant");
        assert!(
            error.contains("I22.retention-boundary.v1 cutoff mismatch"),
            "{error}"
        );
    }

    #[test]
    fn i23_purge_checker_rejects_completed_sentinel_hit() {
        let error = i23_completed_sentinel_hit_plant_error().expect("valid I23 sentinel plant");
        assert!(
            error.contains(
                "I23.purge-proof.v1 completed purge left sentinel in segment-0001.zseg@128"
            ),
            "{error}"
        );
    }

    #[test]
    fn i23_purge_checker_rejects_target_resurrection_after_reopen() {
        let error = i23_resurrection_plant_error().expect("valid I23 resurrection plant");
        assert!(
            error.contains("I23.purge-proof.v1 target 51 resurrected"),
            "{error}"
        );
    }

    #[test]
    fn retained_i20_canonical_bytes_replay_the_exact_checker_and_reject_drift() {
        let baseline = vec![DocumentFact {
            doc_id: 1,
            revision: 1,
            timestamp_witness: vec![true, true],
        }];
        let submitted = vec![DocumentFact {
            doc_id: 2,
            revision: 1,
            timestamp_witness: vec![false, true],
        }];
        let ack = AckFact {
            seq: 2,
            generation: 2,
        };
        let expected = expected_i20(
            &baseline,
            &submitted,
            I20Outcome::Commit,
            Some(ack),
            1,
            None,
        )
        .expect("canonical I20 expected facts");
        let observed = I20Observed {
            typed_result: IngestResultFact::Committed(ack),
            before_live: baseline,
            immediate_live: expected.final_live.clone(),
            stats: I20StatsFact {
                active_rows: 2,
                tombstones: 0,
            },
            post_reopen_live: expected.expected_reopen_live.clone(),
            ack: Some(ack),
            generation_before: 1,
            generation_after: 2,
            retry_ack: None,
            retry_live: None,
            retry_wal_records_appended: None,
            retry_generation_delta: None,
            receipt_digest: None,
        };
        let attestation = attest_i20(&expected, &observed);
        let replay = replay_canonical_comparison(
            I20_CHECKER_ID,
            &attestation.input_bytes,
            &attestation.observed_bytes,
        )
        .expect("replay retained I20 canonical bytes");
        assert!(replay.first_difference.is_none());

        let mut drifted = observed;
        drifted.generation_after = 3;
        let replay = replay_canonical_comparison(
            I20_CHECKER_ID,
            &attestation.input_bytes,
            &canonical_i20_observed_bytes(&drifted),
        )
        .expect("decode retained drifted I20 observation");
        assert!(
            replay
                .first_difference
                .as_ref()
                .is_some_and(|difference| difference.observed.contains("generation delta")),
            "{replay:?}"
        );
    }

    #[test]
    fn retained_i21_canonical_bytes_replay_the_exact_checker_and_reject_drift() {
        let document = DocumentFact {
            doc_id: 21,
            revision: 2,
            timestamp_witness: vec![true, false],
        };
        let expected = I21Expected {
            live_before: vec![document.clone()],
            live_after: vec![document.clone()],
            active_rows_before: 1,
            active_rows_after: 0,
            generation_delta: 1,
        };
        let observed = I21Observed {
            seal_result: SealResultFact::Committed { generation: 2 },
            live_before: vec![document.clone()],
            cancelled_live: None,
            cancelled_active_rows: None,
            cancelled_generation: None,
            cancelled_orphan_paths: None,
            retry_seal_generation: None,
            live_after: vec![document.clone()],
            live_after_reopen: vec![document],
            active_rows_before: 1,
            active_rows_after: 0,
            generation_before: 1,
            generation_after: 2,
            orphan_paths: Vec::new(),
            receipt_digest: None,
        };
        let attestation = attest_i21(&expected, &observed);
        let replay = replay_canonical_comparison(
            I21_CHECKER_ID,
            &attestation.input_bytes,
            &attestation.observed_bytes,
        )
        .expect("replay retained I21 canonical bytes");
        assert!(replay.first_difference.is_none());

        let mut drifted = observed;
        drifted.live_after_reopen.clear();
        let replay = replay_canonical_comparison(
            I21_CHECKER_ID,
            &attestation.input_bytes,
            &canonical_i21_observed_bytes(&drifted),
        )
        .expect("decode retained drifted I21 observation");
        assert!(
            replay
                .first_difference
                .as_ref()
                .is_some_and(|difference| difference.observed.contains("missing document")),
            "{replay:?}"
        );
    }

    #[test]
    fn retained_i22_canonical_bytes_replay_the_exact_checker_and_reject_drift() {
        let document = |doc_id, witnesses| DocumentFact {
            doc_id,
            revision: 1,
            timestamp_witness: witnesses,
        };
        let active = document(4, vec![false, false, true]);
        let expected = expected_i22(
            20,
            10,
            &[
                I22PartitionInput {
                    label: "before".to_owned(),
                    rows: vec![(9, document(1, vec![true, true, true]))],
                },
                I22PartitionInput {
                    label: "at".to_owned(),
                    rows: vec![(10, document(2, vec![false, true, true]))],
                },
                I22PartitionInput {
                    label: "after".to_owned(),
                    rows: vec![(11, document(3, vec![false, false, true]))],
                },
            ],
            active,
        )
        .expect("canonical I22 expected facts");
        let observed = I22Observed {
            supplied_now: 20,
            observed_cutoff: 10,
            observed_drop_range: I22RangeFact {
                start: i64::MIN,
                end: 10,
            },
            generation_before: 4,
            report_generation: 5,
            manifest_committed: true,
            dropped_labels: vec!["before".to_owned()],
            straddler_labels: Vec::new(),
            bytes_reclaimed: 4096,
            live_after: expected.expected_retained_live.clone(),
            live_after_reopen: expected.expected_retained_live.clone(),
            active_control_present: true,
            receipt_digest: None,
        };
        let attestation = attest_i22(&expected, &observed);
        let replay = replay_canonical_comparison(
            I22_CHECKER_ID,
            &attestation.input_bytes,
            &attestation.observed_bytes,
        )
        .expect("replay retained I22 canonical bytes");
        assert!(replay.first_difference.is_none());

        let mut drifted = observed;
        drifted.observed_cutoff = 11;
        let replay = replay_canonical_comparison(
            I22_CHECKER_ID,
            &attestation.input_bytes,
            &canonical_i22_observed_bytes(&drifted),
        )
        .expect("decode retained drifted I22 observation");
        assert!(
            replay
                .first_difference
                .as_ref()
                .is_some_and(|difference| difference.observed.contains("cutoff mismatch")),
            "{replay:?}"
        );
    }

    #[test]
    fn retained_i23_canonical_bytes_replay_the_exact_checker_and_reject_drift() {
        let target = DocumentFact {
            doc_id: 51,
            revision: 2,
            timestamp_witness: vec![true, true],
        };
        let survivor = DocumentFact {
            doc_id: 52,
            revision: 1,
            timestamp_witness: vec![false, true],
        };
        let expected = expected_i23(
            &[target, survivor.clone()],
            51,
            I23TargetLocation::Sealed,
            vec![I23SentinelPattern {
                index: 0,
                bytes: b"purge-sentinel-51".to_vec(),
            }],
        )
        .expect("canonical I23 expected facts");
        let pre_hit = I23SentinelHit {
            relative_path: "segment-0001.zseg".to_owned(),
            offset: 128,
            sentinel_index: 0,
        };
        let observed = I23Observed {
            delete_result: I23DeleteResultFact::Committed(AckFact {
                seq: 3,
                generation: 3,
            }),
            logical_search: vec![survivor.clone()],
            pre_purge_hits: vec![pre_hit.clone()],
            purge_token: I23PurgeTokenFact {
                token_id: 4,
                no_op: false,
            },
            fault_error: None,
            await_result: I23AwaitResultFact {
                completed: true,
                generation: 4,
            },
            post_purge_hits: Vec::new(),
            intent_present: false,
            immediate_live: vec![survivor.clone()],
            reopen_live: vec![survivor.clone()],
            second_reopen_live: vec![survivor],
            generation_facts: I23GenerationFacts {
                before_delete: 2,
                after_delete: 3,
                after_purge: 4,
                after_reopen: 4,
                after_second_reopen: 4,
            },
            receipt_digest: None,
        };
        let attestation = attest_i23(&expected, &observed);
        let replay = replay_canonical_comparison(
            I23_CHECKER_ID,
            &attestation.input_bytes,
            &attestation.observed_bytes,
        )
        .expect("replay retained I23 canonical bytes");
        assert!(replay.first_difference.is_none());

        let mut drifted = observed;
        drifted.post_purge_hits.push(pre_hit);
        let replay = replay_canonical_comparison(
            I23_CHECKER_ID,
            &attestation.input_bytes,
            &canonical_i23_observed_bytes(&drifted),
        )
        .expect("decode retained drifted I23 observation");
        assert!(
            replay
                .first_difference
                .as_ref()
                .is_some_and(|difference| difference.observed.contains("left sentinel")),
            "{replay:?}"
        );
    }
}
