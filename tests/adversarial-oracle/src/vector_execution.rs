//! Independent vector-execution expected values and exact invariant checkers.
//!
//! Inputs and observations use only primitive integers and IEEE bit patterns.
//! No engine type or expected-value helper crosses this boundary.

use std::collections::{BTreeMap, BTreeSet};

pub const VECTOR_ORACLE_CONTRACT: &str = "vector-oracle-v1";
pub const I24_CHECKER_ID: &str = "I24.kernel-contract-parity.v1";
pub const I25_CHECKER_ID: &str = "I25.quantization-contract.v1";
pub const I26_CHECKER_ID: &str = "I26.exact-rescore-contract.v1";
pub const I27_CHECKER_ID: &str = "I27.row-identity-lifecycle.v1";
/// Stable family-owned canonical byte contract used by retained evidence.
pub const VECTOR_CANONICAL_VERSION: &str = "vector-canonical-v1";
const MAX_I8_DIMENSION: usize = 65_536;

/// Exact primitive record encoded by one canonical attestation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorCanonicalKind {
    I24Input,
    I24Observed,
    I25Input,
    I25Observed,
    I26Input,
    I26Observed,
    I27Input,
    I27Observed,
}

impl VectorCanonicalKind {
    const fn tag(self) -> u8 {
        match self {
            Self::I24Input => 1,
            Self::I24Observed => 2,
            Self::I25Input => 3,
            Self::I25Observed => 4,
            Self::I26Input => 5,
            Self::I26Observed => 6,
            Self::I27Input => 7,
            Self::I27Observed => 8,
        }
    }
}

/// Canonical bytes plus their family-owned SHA-256 attestation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorCanonicalRecord {
    pub version: &'static str,
    pub kind: VectorCanonicalKind,
    pub bytes: Vec<u8>,
    pub sha256: [u8; 32],
}

impl VectorCanonicalRecord {
    /// Lowercase fixed-width SHA-256 text for JSON evidence.
    #[must_use]
    pub fn sha256_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(64);
        for byte in self.sha256 {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }
}

/// First semantic disagreement from an exact I24-I27 checker comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorFirstDifference {
    pub checker_id: &'static str,
    pub path: &'static str,
    pub expected: String,
    pub observed: String,
}

struct CanonicalWriter {
    bytes: Vec<u8>,
}

impl CanonicalWriter {
    fn new(kind: VectorCanonicalKind) -> Self {
        let mut bytes = VECTOR_CANONICAL_VERSION.as_bytes().to_vec();
        bytes.push(0);
        bytes.push(kind.tag());
        Self { bytes }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, length: usize) {
        self.u64(length as u64);
    }

    fn bytes(&mut self, value: &[u8]) {
        self.len(value.len());
        self.bytes.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn finish(self, kind: VectorCanonicalKind) -> VectorCanonicalRecord {
        let sha256 = canonical_sha256(&self.bytes);
        VectorCanonicalRecord {
            version: VECTOR_CANONICAL_VERSION,
            kind,
            bytes: self.bytes,
            sha256,
        }
    }
}

fn encode_backend(writer: &mut CanonicalWriter, backend: BackendId) {
    writer.u8(match backend {
        BackendId::Scalar => 0,
        BackendId::NeonWiden => 1,
        BackendId::NeonDotprodU4 => 2,
        BackendId::NeonI8mm => 3,
        BackendId::NeonDotprodU2 => 4,
        BackendId::NeonDotprodU6 => 5,
        BackendId::NeonDotprodU8 => 6,
        BackendId::NeonDotprodU4Prefetch => 7,
        BackendId::Avx2 => 8,
    });
}

fn encode_kernel(writer: &mut CanonicalWriter, kernel: KernelId) {
    writer.u8(match kernel {
        KernelId::DotI8 => 0,
        KernelId::HammingU1 => 1,
        KernelId::DotF32 => 2,
        KernelId::DotF16 => 3,
        KernelId::DotI8Batch => 4,
        KernelId::HammingU1Batch => 5,
        KernelId::DotBit4 => 6,
        KernelId::DotBit4Prepared => 7,
        KernelId::DotBit4Batch => 8,
        KernelId::ScoreBit4PreparedBatch => 9,
        KernelId::ScoreBit4Ptrs => 10,
    });
}

fn encode_document(writer: &mut CanonicalWriter, document: PrimitiveDocument) {
    writer.bytes.extend_from_slice(&document.doc_id_be);
    writer.u64(document.revision);
}

fn encode_source(writer: &mut CanonicalWriter, source: PrimitiveSource) {
    match source {
        PrimitiveSource::Active => writer.u8(0),
        PrimitiveSource::Sealed(segment) => {
            writer.u8(1);
            writer.bytes.extend_from_slice(&segment);
        }
    }
}

fn encode_row(writer: &mut CanonicalWriter, row: PrimitiveRow) {
    encode_source(writer, row.source);
    writer.u32(row.local_row);
}

fn encode_status(writer: &mut CanonicalWriter, status: &PrimitiveStatus) {
    match status {
        PrimitiveStatus::Ok => writer.u8(0),
        PrimitiveStatus::EmptyVector => writer.u8(1),
        PrimitiveStatus::DimensionTooLarge { actual, maximum } => {
            writer.u8(2);
            writer.u64(*actual);
            writer.u64(*maximum);
        }
        PrimitiveStatus::NonFinite { index } => {
            writer.u8(3);
            writer.u64(*index);
        }
        PrimitiveStatus::OutputLength { expected, actual } => {
            writer.u8(4);
            writer.u64(*expected);
            writer.u64(*actual);
        }
        PrimitiveStatus::CodeLength { expected, actual } => {
            writer.u8(5);
            writer.u64(*expected);
            writer.u64(*actual);
        }
        PrimitiveStatus::NonZeroPadding { byte, mask } => {
            writer.u8(6);
            writer.u8(*byte);
            writer.u8(*mask);
        }
        PrimitiveStatus::CandidateRowCount { expected, actual } => {
            writer.u8(7);
            writer.u64(*expected);
            writer.u64(*actual);
        }
        PrimitiveStatus::CandidateRowOutOfRange { row, rows } => {
            writer.u8(8);
            writer.u64(*row);
            writer.u64(*rows);
        }
        PrimitiveStatus::NonFiniteScore { row } => {
            writer.u8(9);
            writer.u64(*row);
        }
        PrimitiveStatus::Cancelled { partial } => {
            writer.u8(10);
            writer.bool(*partial);
        }
        PrimitiveStatus::AllocationFailed { component, needed } => {
            writer.u8(11);
            writer.string(component);
            writer.u64(*needed);
        }
        PrimitiveStatus::SegmentGeometry { detail } => {
            writer.u8(12);
            writer.string(detail);
        }
    }
}

fn encode_store_step(writer: &mut CanonicalWriter, step: PublicStoreStep) {
    writer.u8(match step {
        PublicStoreStep::IngestAccepted => 0,
        PublicStoreStep::IngestRejected => 1,
        PublicStoreStep::Seal => 2,
        PublicStoreStep::PublishPreparedSegment => 3,
        PublicStoreStep::DeleteAccepted => 4,
        PublicStoreStep::Reopen => 5,
        PublicStoreStep::Search => 6,
    });
}

fn encode_kernel_value(writer: &mut CanonicalWriter, value: &KernelValue) {
    match value {
        KernelValue::S32(value) => {
            writer.u8(0);
            writer.i32(*value);
        }
        KernelValue::U32(value) => {
            writer.u8(1);
            writer.u32(*value);
        }
        KernelValue::F32(value) => {
            writer.u8(2);
            writer.u32(value.0);
        }
        KernelValue::S32s(values) => {
            writer.u8(3);
            writer.len(values.len());
            for value in values {
                writer.i32(*value);
            }
        }
        KernelValue::U32s(values) => {
            writer.u8(4);
            writer.len(values.len());
            for value in values {
                writer.u32(*value);
            }
        }
        KernelValue::F32s(values) => {
            writer.u8(5);
            writer.len(values.len());
            for value in values {
                writer.u32(value.0);
            }
        }
    }
}

fn encode_kernel_input(writer: &mut CanonicalWriter, input: &KernelInput) {
    writer.u64(input.case_id);
    encode_backend(writer, input.backend);
    writer.bool(input.selected_for_store);
    writer.u64(input.work_items);
    encode_kernel(writer, input.kernel);
    writer.u64(input.dimension);
    writer.len(input.signed_a.len());
    for value in &input.signed_a {
        writer.u8(*value as u8);
    }
    writer.len(input.signed_b.len());
    for value in &input.signed_b {
        writer.u8(*value as u8);
    }
    writer.bytes(&input.bytes_a);
    writer.bytes(&input.bytes_b);
    writer.len(input.f32_a.len());
    for value in &input.f32_a {
        writer.u32(value.0);
    }
    writer.len(input.f32_b.len());
    for value in &input.f32_b {
        writer.u32(value.0);
    }
    writer.len(input.f16_a.len());
    for value in &input.f16_a {
        writer.u16(*value);
    }
    writer.len(input.f16_b.len());
    for value in &input.f16_b {
        writer.u16(*value);
    }
    writer.u64(input.row_bytes);
    writer.u64(input.batch_rows);
    writer.bytes(&input.pointer_order);
    writer.i32(input.query_sum);
    writer.u64(input.query_scale_half.0);
    writer.len(input.bit4_factors.len());
    for factors in &input.bit4_factors {
        for factor in factors {
            writer.u32(factor.0);
        }
    }
}

fn encode_quant_scheme(writer: &mut CanonicalWriter, scheme: QuantScheme) {
    writer.u8(match scheme {
        QuantScheme::Bit4 => 0,
        QuantScheme::Int8 => 1,
    });
}

fn encode_quant_input(writer: &mut CanonicalWriter, input: &QuantInput) {
    writer.u64(input.case_id);
    encode_quant_scheme(writer, input.scheme);
    writer.len(input.row.len());
    for value in &input.row {
        writer.u32(value.0);
    }
    writer.len(input.query.len());
    for value in &input.query {
        writer.u32(value.0);
    }
    writer.u64(input.query_seed);
    writer.u64(input.output_len);
    writer.u64(input.code_len);
    writer.u8(input.sentinel);
    writer.u64(input.store.generation_before);
    writer.len(input.store.schedule.len());
    for step in &input.store.schedule {
        encode_store_step(writer, *step);
    }
    match input.store.document {
        Some(document) => {
            writer.bool(true);
            encode_document(writer, document);
        }
        None => writer.bool(false),
    }
    writer.bool(input.store.document_visible);
}

fn encode_quant_success(writer: &mut CanonicalWriter, success: &QuantSuccess) {
    writer.bytes(&success.code_bytes);
    writer.len(success.factor_bits.len());
    for value in &success.factor_bits {
        writer.u32(value.0);
    }
    writer.bytes(&success.query_code_bytes);
    writer.i32(success.query_code_sum);
    writer.u64(success.query_scale.0);
    writer.len(success.reconstruction.len());
    for value in &success.reconstruction {
        writer.u32(value.0);
    }
    writer.u32(success.estimate.0);
}

fn encode_quant_result(writer: &mut CanonicalWriter, result: &QuantResult) {
    encode_status(writer, &result.status);
    writer.bytes(&result.output_after);
    match &result.success {
        Some(success) => {
            writer.bool(true);
            encode_quant_success(writer, success);
        }
        None => writer.bool(false),
    }
}

fn encode_quant_store_facts(writer: &mut CanonicalWriter, facts: &QuantStoreFacts) {
    encode_status(writer, &facts.ingest_status);
    encode_status(writer, &facts.scan_status);
    writer.u64(facts.generation_before);
    writer.u64(facts.generation_after);
    writer.bool(facts.document_visible);
    writer.bytes(&facts.persisted_code_bytes);
    writer.len(facts.persisted_factor_bits.len());
    for value in &facts.persisted_factor_bits {
        writer.u32(value.0);
    }
}

fn encode_rescore_metric(writer: &mut CanonicalWriter, metric: RescoreMetric) {
    writer.u8(match metric {
        RescoreMetric::InnerProduct => 0,
        RescoreMetric::SquaredL2 => 1,
    });
}

fn encode_candidate_mode(writer: &mut CanonicalWriter, candidates: &CandidateMode) {
    match candidates {
        CandidateMode::Dense { coarse, oversample } => {
            writer.u8(0);
            writer.len(coarse.len());
            for value in coarse {
                writer.u32(value.0);
            }
            writer.u64(*oversample);
        }
        CandidateMode::Retained { rows, coarse } => {
            writer.u8(1);
            writer.len(rows.len());
            for row in rows {
                writer.u32(*row);
            }
            writer.len(coarse.len());
            for value in coarse {
                writer.u32(value.0);
            }
        }
    }
}

fn encode_store_hit(writer: &mut CanonicalWriter, hit: &StoreRescoreHit) {
    encode_row(writer, hit.row);
    match hit.document {
        Some(document) => {
            writer.bool(true);
            encode_document(writer, document);
        }
        None => writer.bool(false),
    }
    writer.u32(hit.score.0);
    writer.bool(hit.exact_score);
}

fn encode_rescore_input(writer: &mut CanonicalWriter, input: &RescoreInput) {
    writer.u64(input.case_id);
    encode_rescore_metric(writer, input.metric);
    writer.len(input.query.len());
    for value in &input.query {
        writer.u32(value.0);
    }
    writer.len(input.rows_row_major.len());
    for value in &input.rows_row_major {
        writer.u32(value.0);
    }
    writer.u64(input.dimension);
    writer.u64(input.k);
    encode_candidate_mode(writer, &input.candidates);
    writer.u64(input.coarse_rows_touched);
    writer.u64(input.coarse_bytes_per_row);
    match &input.store {
        Some(store) => {
            writer.bool(true);
            encode_source(writer, store.source);
            writer.len(store.documents_by_row.len());
            for document in &store.documents_by_row {
                match document {
                    Some(document) => {
                        writer.bool(true);
                        encode_document(writer, *document);
                    }
                    None => writer.bool(false),
                }
            }
            writer.u8(store.tier);
            writer.bool(store.exact_rescore);
        }
        None => writer.bool(false),
    }
}

fn encode_identity_row(writer: &mut CanonicalWriter, row: &IdentityObservedRow) {
    encode_row(writer, row.row);
    match row.document {
        Some(document) => {
            writer.bool(true);
            encode_document(writer, document);
        }
        None => writer.bool(false),
    }
    writer.u32(row.score.0);
}

fn encode_identity_input(writer: &mut CanonicalWriter, input: &IdentityInput) {
    writer.u64(input.case_id);
    writer.len(input.mutations.len());
    for mutation in &input.mutations {
        match mutation {
            IdentityMutation::Ingest(document) => {
                writer.u8(0);
                encode_document(writer, *document);
            }
            IdentityMutation::Seal(segment) => {
                writer.u8(1);
                writer.bytes.extend_from_slice(segment);
            }
            IdentityMutation::Reopen => writer.u8(2),
            IdentityMutation::Replace(document) => {
                writer.u8(3);
                encode_document(writer, *document);
            }
            IdentityMutation::Delete {
                doc_id_be,
                revision,
            } => {
                writer.u8(4);
                writer.bytes.extend_from_slice(doc_id_be);
                writer.u64(*revision);
            }
        }
    }
    writer.len(input.query.len());
    for value in &input.query {
        writer.u32(value.0);
    }
    writer.u64(input.k);
    writer.u8(input.tier);
    writer.u8(input.observation_phase);
    writer.len(input.public_schedule.len());
    for step in &input.public_schedule {
        encode_store_step(writer, *step);
    }
}

fn canonical_record(
    kind: VectorCanonicalKind,
    encode: impl FnOnce(&mut CanonicalWriter),
) -> VectorCanonicalRecord {
    let mut writer = CanonicalWriter::new(kind);
    encode(&mut writer);
    writer.finish(kind)
}

#[must_use]
pub fn canonical_i24_input(input: &KernelInput) -> VectorCanonicalRecord {
    canonical_record(VectorCanonicalKind::I24Input, |writer| {
        encode_kernel_input(writer, input);
    })
}

#[must_use]
pub fn canonical_i24_observed(observed: &I24Observed) -> VectorCanonicalRecord {
    canonical_record(VectorCanonicalKind::I24Observed, |writer| {
        writer.u64(observed.case_id);
        encode_backend(writer, observed.backend);
        encode_kernel(writer, observed.kernel);
        encode_kernel_value(writer, &observed.value);
        writer.bool(observed.selected_for_store);
        writer.u64(observed.work_items);
    })
}

#[must_use]
pub fn canonical_i25_input(input: &QuantInput) -> VectorCanonicalRecord {
    canonical_record(VectorCanonicalKind::I25Input, |writer| {
        encode_quant_input(writer, input);
    })
}

#[must_use]
pub fn canonical_i25_observed(observed: &I25Observed) -> VectorCanonicalRecord {
    canonical_record(VectorCanonicalKind::I25Observed, |writer| {
        writer.u64(observed.case_id);
        encode_quant_result(writer, &observed.result);
        encode_quant_store_facts(writer, &observed.store);
    })
}

#[must_use]
pub fn canonical_i26_input(input: &RescoreInput) -> VectorCanonicalRecord {
    canonical_record(VectorCanonicalKind::I26Input, |writer| {
        encode_rescore_input(writer, input);
    })
}

#[must_use]
pub fn canonical_i26_observed(observed: &I26Observed) -> VectorCanonicalRecord {
    canonical_record(VectorCanonicalKind::I26Observed, |writer| {
        writer.u64(observed.case_id);
        encode_status(writer, &observed.primitive_status);
        writer.len(observed.primitive_hits.len());
        for hit in &observed.primitive_hits {
            writer.u32(hit.row);
            writer.u64(hit.score.0);
        }
        for count in observed.primitive_counts {
            writer.u64(count);
        }
        writer.u8(observed.tier);
        writer.len(observed.store_hits.len());
        for hit in &observed.store_hits {
            encode_store_hit(writer, hit);
        }
        writer.bool(observed.exact_rescore);
    })
}

#[must_use]
pub fn canonical_i27_input(input: &IdentityInput) -> VectorCanonicalRecord {
    canonical_record(VectorCanonicalKind::I27Input, |writer| {
        encode_identity_input(writer, input);
    })
}

#[must_use]
pub fn canonical_i27_observed(observed: &I27Observed) -> VectorCanonicalRecord {
    canonical_record(VectorCanonicalKind::I27Observed, |writer| {
        writer.u64(observed.case_id);
        writer.len(observed.rows.len());
        for row in &observed.rows {
            encode_identity_row(writer, row);
        }
        writer.u64(observed.generation);
        writer.u8(observed.phase);
        writer.u8(observed.tier);
        writer.len(observed.control_rows.len());
        for row in &observed.control_rows {
            encode_identity_row(writer, row);
        }
        writer.u64(observed.control_generation);
        writer.len(observed.retry_rows.len());
        for row in &observed.retry_rows {
            encode_identity_row(writer, row);
        }
        writer.u64(observed.retry_generation);
        encode_status(writer, &observed.fault_status);
        encode_status(writer, &observed.retry_status);
    })
}

/// Result of executing one checker directly from retained canonical bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorCanonicalReplay {
    pub checker_id: &'static str,
    pub case_id: u64,
    pub input_sha256: [u8; 32],
    pub observed_sha256: [u8; 32],
    pub first_difference: Option<VectorFirstDifference>,
}

impl VectorCanonicalReplay {
    #[must_use]
    pub const fn first_difference(&self) -> Option<&VectorFirstDifference> {
        self.first_difference.as_ref()
    }
}

struct CanonicalReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> CanonicalReader<'a> {
    fn new(bytes: &'a [u8], kind: VectorCanonicalKind) -> Result<Self, String> {
        let prefix = VECTOR_CANONICAL_VERSION.as_bytes();
        let header = prefix
            .len()
            .checked_add(2)
            .ok_or_else(|| "vector canonical header length overflow".to_owned())?;
        if bytes.get(..prefix.len()) != Some(prefix)
            || bytes.get(prefix.len()) != Some(&0)
            || bytes.get(prefix.len() + 1) != Some(&kind.tag())
        {
            return Err(format!(
                "retained vector canonical header is not {VECTOR_CANONICAL_VERSION}/{kind:?}"
            ));
        }
        Ok(Self {
            bytes,
            position: header,
        })
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| "retained vector canonical range overflow".to_owned())?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| "retained vector canonical record is truncated".to_owned())?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, String> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn bool(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(format!(
                "retained vector canonical bool has invalid tag {value}"
            )),
        }
    }

    fn u16(&mut self) -> Result<u16, String> {
        self.take(2)?
            .try_into()
            .map(u16::from_le_bytes)
            .map_err(|_| "retained vector canonical u16 width changed".to_owned())
    }

    fn u32(&mut self) -> Result<u32, String> {
        self.take(4)?
            .try_into()
            .map(u32::from_le_bytes)
            .map_err(|_| "retained vector canonical u32 width changed".to_owned())
    }

    fn i32(&mut self) -> Result<i32, String> {
        self.take(4)?
            .try_into()
            .map(i32::from_le_bytes)
            .map_err(|_| "retained vector canonical i32 width changed".to_owned())
    }

    fn u64(&mut self) -> Result<u64, String> {
        self.take(8)?
            .try_into()
            .map(u64::from_le_bytes)
            .map_err(|_| "retained vector canonical u64 width changed".to_owned())
    }

    fn len(&mut self, minimum_width: usize) -> Result<usize, String> {
        let length = usize::try_from(self.u64()?)
            .map_err(|_| "retained vector canonical length exceeds usize".to_owned())?;
        let remaining = self.bytes.len().saturating_sub(self.position);
        if minimum_width > 0 && length > remaining / minimum_width {
            return Err("retained vector canonical collection length is impossible".to_owned());
        }
        Ok(length)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let length = self.len(1)?;
        self.take(length).map(<[u8]>::to_vec)
    }

    fn string(&mut self) -> Result<String, String> {
        String::from_utf8(self.bytes()?)
            .map_err(|_| "retained vector canonical string is not UTF-8".to_owned())
    }

    fn array16(&mut self) -> Result<[u8; 16], String> {
        self.take(16)?
            .try_into()
            .map_err(|_| "retained vector canonical 16-byte field changed width".to_owned())
    }

    fn finish(self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "retained vector canonical record has {} trailing bytes",
                self.bytes.len() - self.position
            ))
        }
    }
}

fn canonical_kind(bytes: &[u8]) -> Result<VectorCanonicalKind, String> {
    let position = VECTOR_CANONICAL_VERSION
        .len()
        .checked_add(1)
        .ok_or_else(|| "vector canonical tag offset overflow".to_owned())?;
    if bytes.get(..VECTOR_CANONICAL_VERSION.len()) != Some(VECTOR_CANONICAL_VERSION.as_bytes())
        || bytes.get(VECTOR_CANONICAL_VERSION.len()) != Some(&0)
    {
        return Err("retained vector canonical version prefix differs".to_owned());
    }
    match bytes.get(position).copied() {
        Some(1) => Ok(VectorCanonicalKind::I24Input),
        Some(2) => Ok(VectorCanonicalKind::I24Observed),
        Some(3) => Ok(VectorCanonicalKind::I25Input),
        Some(4) => Ok(VectorCanonicalKind::I25Observed),
        Some(5) => Ok(VectorCanonicalKind::I26Input),
        Some(6) => Ok(VectorCanonicalKind::I26Observed),
        Some(7) => Ok(VectorCanonicalKind::I27Input),
        Some(8) => Ok(VectorCanonicalKind::I27Observed),
        Some(tag) => Err(format!(
            "retained vector canonical kind tag {tag} is unknown"
        )),
        None => Err("retained vector canonical record has no kind tag".to_owned()),
    }
}

fn decode_backend(reader: &mut CanonicalReader<'_>) -> Result<BackendId, String> {
    match reader.u8()? {
        0 => Ok(BackendId::Scalar),
        1 => Ok(BackendId::NeonWiden),
        2 => Ok(BackendId::NeonDotprodU4),
        3 => Ok(BackendId::NeonI8mm),
        4 => Ok(BackendId::NeonDotprodU2),
        5 => Ok(BackendId::NeonDotprodU6),
        6 => Ok(BackendId::NeonDotprodU8),
        7 => Ok(BackendId::NeonDotprodU4Prefetch),
        8 => Ok(BackendId::Avx2),
        tag => Err(format!("retained vector backend tag {tag} is unknown")),
    }
}

fn decode_kernel(reader: &mut CanonicalReader<'_>) -> Result<KernelId, String> {
    match reader.u8()? {
        0 => Ok(KernelId::DotI8),
        1 => Ok(KernelId::HammingU1),
        2 => Ok(KernelId::DotF32),
        3 => Ok(KernelId::DotF16),
        4 => Ok(KernelId::DotI8Batch),
        5 => Ok(KernelId::HammingU1Batch),
        6 => Ok(KernelId::DotBit4),
        7 => Ok(KernelId::DotBit4Prepared),
        8 => Ok(KernelId::DotBit4Batch),
        9 => Ok(KernelId::ScoreBit4PreparedBatch),
        10 => Ok(KernelId::ScoreBit4Ptrs),
        tag => Err(format!("retained vector kernel tag {tag} is unknown")),
    }
}

fn decode_document(reader: &mut CanonicalReader<'_>) -> Result<PrimitiveDocument, String> {
    Ok(PrimitiveDocument {
        doc_id_be: reader.array16()?,
        revision: reader.u64()?,
    })
}

fn decode_source(reader: &mut CanonicalReader<'_>) -> Result<PrimitiveSource, String> {
    match reader.u8()? {
        0 => Ok(PrimitiveSource::Active),
        1 => Ok(PrimitiveSource::Sealed(reader.array16()?)),
        tag => Err(format!("retained vector source tag {tag} is unknown")),
    }
}

fn decode_row(reader: &mut CanonicalReader<'_>) -> Result<PrimitiveRow, String> {
    Ok(PrimitiveRow {
        source: decode_source(reader)?,
        local_row: reader.u32()?,
    })
}

fn decode_status(reader: &mut CanonicalReader<'_>) -> Result<PrimitiveStatus, String> {
    match reader.u8()? {
        0 => Ok(PrimitiveStatus::Ok),
        1 => Ok(PrimitiveStatus::EmptyVector),
        2 => Ok(PrimitiveStatus::DimensionTooLarge {
            actual: reader.u64()?,
            maximum: reader.u64()?,
        }),
        3 => Ok(PrimitiveStatus::NonFinite {
            index: reader.u64()?,
        }),
        4 => Ok(PrimitiveStatus::OutputLength {
            expected: reader.u64()?,
            actual: reader.u64()?,
        }),
        5 => Ok(PrimitiveStatus::CodeLength {
            expected: reader.u64()?,
            actual: reader.u64()?,
        }),
        6 => Ok(PrimitiveStatus::NonZeroPadding {
            byte: reader.u8()?,
            mask: reader.u8()?,
        }),
        7 => Ok(PrimitiveStatus::CandidateRowCount {
            expected: reader.u64()?,
            actual: reader.u64()?,
        }),
        8 => Ok(PrimitiveStatus::CandidateRowOutOfRange {
            row: reader.u64()?,
            rows: reader.u64()?,
        }),
        9 => Ok(PrimitiveStatus::NonFiniteScore { row: reader.u64()? }),
        10 => Ok(PrimitiveStatus::Cancelled {
            partial: reader.bool()?,
        }),
        11 => Ok(PrimitiveStatus::AllocationFailed {
            component: reader.string()?,
            needed: reader.u64()?,
        }),
        12 => Ok(PrimitiveStatus::SegmentGeometry {
            detail: reader.string()?,
        }),
        tag => Err(format!("retained vector status tag {tag} is unknown")),
    }
}

fn decode_store_step(reader: &mut CanonicalReader<'_>) -> Result<PublicStoreStep, String> {
    match reader.u8()? {
        0 => Ok(PublicStoreStep::IngestAccepted),
        1 => Ok(PublicStoreStep::IngestRejected),
        2 => Ok(PublicStoreStep::Seal),
        3 => Ok(PublicStoreStep::PublishPreparedSegment),
        4 => Ok(PublicStoreStep::DeleteAccepted),
        5 => Ok(PublicStoreStep::Reopen),
        6 => Ok(PublicStoreStep::Search),
        tag => Err(format!("retained vector Store-step tag {tag} is unknown")),
    }
}

fn decode_kernel_value(reader: &mut CanonicalReader<'_>) -> Result<KernelValue, String> {
    match reader.u8()? {
        0 => Ok(KernelValue::S32(reader.i32()?)),
        1 => Ok(KernelValue::U32(reader.u32()?)),
        2 => Ok(KernelValue::F32(F32(reader.u32()?))),
        3 => {
            let length = reader.len(4)?;
            (0..length)
                .map(|_| reader.i32())
                .collect::<Result<Vec<_>, _>>()
                .map(KernelValue::S32s)
        }
        4 => {
            let length = reader.len(4)?;
            (0..length)
                .map(|_| reader.u32())
                .collect::<Result<Vec<_>, _>>()
                .map(KernelValue::U32s)
        }
        5 => {
            let length = reader.len(4)?;
            (0..length)
                .map(|_| reader.u32().map(F32))
                .collect::<Result<Vec<_>, _>>()
                .map(KernelValue::F32s)
        }
        tag => Err(format!("retained vector kernel-value tag {tag} is unknown")),
    }
}

fn decode_i24_input(bytes: &[u8]) -> Result<KernelInput, String> {
    let mut reader = CanonicalReader::new(bytes, VectorCanonicalKind::I24Input)?;
    let case_id = reader.u64()?;
    let backend = decode_backend(&mut reader)?;
    let selected_for_store = reader.bool()?;
    let work_items = reader.u64()?;
    let kernel = decode_kernel(&mut reader)?;
    let dimension = reader.u64()?;
    let signed_a_len = reader.len(1)?;
    let signed_a = (0..signed_a_len)
        .map(|_| reader.u8().map(|value| value as i8))
        .collect::<Result<Vec<_>, _>>()?;
    let signed_b_len = reader.len(1)?;
    let signed_b = (0..signed_b_len)
        .map(|_| reader.u8().map(|value| value as i8))
        .collect::<Result<Vec<_>, _>>()?;
    let bytes_a = reader.bytes()?;
    let bytes_b = reader.bytes()?;
    let f32_a_len = reader.len(4)?;
    let f32_a = (0..f32_a_len)
        .map(|_| reader.u32().map(F32))
        .collect::<Result<Vec<_>, _>>()?;
    let f32_b_len = reader.len(4)?;
    let f32_b = (0..f32_b_len)
        .map(|_| reader.u32().map(F32))
        .collect::<Result<Vec<_>, _>>()?;
    let f16_a_len = reader.len(2)?;
    let f16_a = (0..f16_a_len)
        .map(|_| reader.u16())
        .collect::<Result<Vec<_>, _>>()?;
    let f16_b_len = reader.len(2)?;
    let f16_b = (0..f16_b_len)
        .map(|_| reader.u16())
        .collect::<Result<Vec<_>, _>>()?;
    let row_bytes = reader.u64()?;
    let batch_rows = reader.u64()?;
    let pointer_order = reader.bytes()?;
    let query_sum = reader.i32()?;
    let query_scale_half = F64(reader.u64()?);
    let factors_len = reader.len(12)?;
    let bit4_factors = (0..factors_len)
        .map(|_| Ok([F32(reader.u32()?), F32(reader.u32()?), F32(reader.u32()?)]))
        .collect::<Result<Vec<_>, String>>()?;
    reader.finish()?;
    Ok(KernelInput {
        case_id,
        backend,
        selected_for_store,
        work_items,
        kernel,
        dimension,
        signed_a,
        signed_b,
        bytes_a,
        bytes_b,
        f32_a,
        f32_b,
        f16_a,
        f16_b,
        row_bytes,
        batch_rows,
        pointer_order,
        query_sum,
        query_scale_half,
        bit4_factors,
    })
}

fn decode_i24_observed(bytes: &[u8]) -> Result<I24Observed, String> {
    let mut reader = CanonicalReader::new(bytes, VectorCanonicalKind::I24Observed)?;
    let observed = I24Observed {
        case_id: reader.u64()?,
        backend: decode_backend(&mut reader)?,
        kernel: decode_kernel(&mut reader)?,
        value: decode_kernel_value(&mut reader)?,
        selected_for_store: reader.bool()?,
        work_items: reader.u64()?,
    };
    reader.finish()?;
    Ok(observed)
}

fn decode_quant_scheme(reader: &mut CanonicalReader<'_>) -> Result<QuantScheme, String> {
    match reader.u8()? {
        0 => Ok(QuantScheme::Bit4),
        1 => Ok(QuantScheme::Int8),
        tag => Err(format!("retained vector quant scheme tag {tag} is unknown")),
    }
}

fn decode_f32s(reader: &mut CanonicalReader<'_>) -> Result<Vec<F32>, String> {
    let length = reader.len(4)?;
    (0..length)
        .map(|_| reader.u32().map(F32))
        .collect::<Result<Vec<_>, _>>()
}

fn decode_steps(reader: &mut CanonicalReader<'_>) -> Result<Vec<PublicStoreStep>, String> {
    let length = reader.len(1)?;
    (0..length)
        .map(|_| decode_store_step(reader))
        .collect::<Result<Vec<_>, _>>()
}

fn decode_optional_document(
    reader: &mut CanonicalReader<'_>,
) -> Result<Option<PrimitiveDocument>, String> {
    if reader.bool()? {
        decode_document(reader).map(Some)
    } else {
        Ok(None)
    }
}

fn decode_i25_input(bytes: &[u8]) -> Result<QuantInput, String> {
    let mut reader = CanonicalReader::new(bytes, VectorCanonicalKind::I25Input)?;
    let input = QuantInput {
        case_id: reader.u64()?,
        scheme: decode_quant_scheme(&mut reader)?,
        row: decode_f32s(&mut reader)?,
        query: decode_f32s(&mut reader)?,
        query_seed: reader.u64()?,
        output_len: reader.u64()?,
        code_len: reader.u64()?,
        sentinel: reader.u8()?,
        store: QuantStoreInput {
            generation_before: reader.u64()?,
            schedule: decode_steps(&mut reader)?,
            document: decode_optional_document(&mut reader)?,
            document_visible: reader.bool()?,
        },
    };
    reader.finish()?;
    Ok(input)
}

fn decode_quant_success(reader: &mut CanonicalReader<'_>) -> Result<QuantSuccess, String> {
    let code_bytes = reader.bytes()?;
    let factor_bits = decode_f32s(reader)?;
    let query_code_bytes = reader.bytes()?;
    let query_code_sum = reader.i32()?;
    let query_scale = F64(reader.u64()?);
    let reconstruction = decode_f32s(reader)?;
    let estimate = F32(reader.u32()?);
    Ok(QuantSuccess {
        code_bytes,
        factor_bits,
        query_code_bytes,
        query_code_sum,
        query_scale,
        reconstruction,
        estimate,
    })
}

fn decode_quant_result(reader: &mut CanonicalReader<'_>) -> Result<QuantResult, String> {
    Ok(QuantResult {
        status: decode_status(reader)?,
        output_after: reader.bytes()?,
        success: if reader.bool()? {
            Some(decode_quant_success(reader)?)
        } else {
            None
        },
    })
}

fn decode_i25_observed(bytes: &[u8]) -> Result<I25Observed, String> {
    let mut reader = CanonicalReader::new(bytes, VectorCanonicalKind::I25Observed)?;
    let case_id = reader.u64()?;
    let result = decode_quant_result(&mut reader)?;
    let ingest_status = decode_status(&mut reader)?;
    let scan_status = decode_status(&mut reader)?;
    let generation_before = reader.u64()?;
    let generation_after = reader.u64()?;
    let document_visible = reader.bool()?;
    let persisted_code_bytes = reader.bytes()?;
    let persisted_factor_bits = decode_f32s(&mut reader)?;
    reader.finish()?;
    Ok(I25Observed {
        case_id,
        result,
        store: QuantStoreFacts {
            ingest_status,
            scan_status,
            generation_before,
            generation_after,
            document_visible,
            persisted_code_bytes,
            persisted_factor_bits,
        },
    })
}

fn decode_rescore_metric(reader: &mut CanonicalReader<'_>) -> Result<RescoreMetric, String> {
    match reader.u8()? {
        0 => Ok(RescoreMetric::InnerProduct),
        1 => Ok(RescoreMetric::SquaredL2),
        tag => Err(format!(
            "retained vector rescore metric tag {tag} is unknown"
        )),
    }
}

fn decode_candidate_mode(reader: &mut CanonicalReader<'_>) -> Result<CandidateMode, String> {
    match reader.u8()? {
        0 => Ok(CandidateMode::Dense {
            coarse: decode_f32s(reader)?,
            oversample: reader.u64()?,
        }),
        1 => {
            let rows_len = reader.len(4)?;
            let rows = (0..rows_len)
                .map(|_| reader.u32())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CandidateMode::Retained {
                rows,
                coarse: decode_f32s(reader)?,
            })
        }
        tag => Err(format!(
            "retained vector candidate mode tag {tag} is unknown"
        )),
    }
}

fn decode_i26_input(bytes: &[u8]) -> Result<RescoreInput, String> {
    let mut reader = CanonicalReader::new(bytes, VectorCanonicalKind::I26Input)?;
    let case_id = reader.u64()?;
    let metric = decode_rescore_metric(&mut reader)?;
    let query = decode_f32s(&mut reader)?;
    let rows_row_major = decode_f32s(&mut reader)?;
    let dimension = reader.u64()?;
    let k = reader.u64()?;
    let candidates = decode_candidate_mode(&mut reader)?;
    let coarse_rows_touched = reader.u64()?;
    let coarse_bytes_per_row = reader.u64()?;
    let store = if reader.bool()? {
        let source = decode_source(&mut reader)?;
        let documents_len = reader.len(1)?;
        let documents_by_row = (0..documents_len)
            .map(|_| decode_optional_document(&mut reader))
            .collect::<Result<Vec<_>, _>>()?;
        Some(RescoreStoreInput {
            source,
            documents_by_row,
            tier: reader.u8()?,
            exact_rescore: reader.bool()?,
        })
    } else {
        None
    };
    reader.finish()?;
    Ok(RescoreInput {
        case_id,
        metric,
        query,
        rows_row_major,
        dimension,
        k,
        candidates,
        coarse_rows_touched,
        coarse_bytes_per_row,
        store,
    })
}

fn decode_store_hit(reader: &mut CanonicalReader<'_>) -> Result<StoreRescoreHit, String> {
    Ok(StoreRescoreHit {
        row: decode_row(reader)?,
        document: decode_optional_document(reader)?,
        score: F32(reader.u32()?),
        exact_score: reader.bool()?,
    })
}

fn decode_i26_observed(bytes: &[u8]) -> Result<I26Observed, String> {
    let mut reader = CanonicalReader::new(bytes, VectorCanonicalKind::I26Observed)?;
    let case_id = reader.u64()?;
    let primitive_status = decode_status(&mut reader)?;
    let hits_len = reader.len(12)?;
    let primitive_hits = (0..hits_len)
        .map(|_| {
            Ok(PrimitiveRescoreHit {
                row: reader.u32()?,
                score: F64(reader.u64()?),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let primitive_counts = [reader.u64()?, reader.u64()?, reader.u64()?, reader.u64()?];
    let tier = reader.u8()?;
    let store_hits_len = reader.len(1)?;
    let store_hits = (0..store_hits_len)
        .map(|_| decode_store_hit(&mut reader))
        .collect::<Result<Vec<_>, _>>()?;
    let exact_rescore = reader.bool()?;
    reader.finish()?;
    Ok(I26Observed {
        case_id,
        primitive_status,
        primitive_hits,
        primitive_counts,
        tier,
        store_hits,
        exact_rescore,
    })
}

fn decode_identity_row(reader: &mut CanonicalReader<'_>) -> Result<IdentityObservedRow, String> {
    Ok(IdentityObservedRow {
        row: decode_row(reader)?,
        document: decode_optional_document(reader)?,
        score: F32(reader.u32()?),
    })
}

fn decode_identity_rows(
    reader: &mut CanonicalReader<'_>,
) -> Result<Vec<IdentityObservedRow>, String> {
    let length = reader.len(1)?;
    (0..length)
        .map(|_| decode_identity_row(reader))
        .collect::<Result<Vec<_>, _>>()
}

fn decode_i27_input(bytes: &[u8]) -> Result<IdentityInput, String> {
    let mut reader = CanonicalReader::new(bytes, VectorCanonicalKind::I27Input)?;
    let case_id = reader.u64()?;
    let mutations_len = reader.len(1)?;
    let mutations = (0..mutations_len)
        .map(|_| match reader.u8()? {
            0 => decode_document(&mut reader).map(IdentityMutation::Ingest),
            1 => reader.array16().map(IdentityMutation::Seal),
            2 => Ok(IdentityMutation::Reopen),
            3 => decode_document(&mut reader).map(IdentityMutation::Replace),
            4 => Ok(IdentityMutation::Delete {
                doc_id_be: reader.array16()?,
                revision: reader.u64()?,
            }),
            tag => Err(format!(
                "retained vector identity mutation tag {tag} is unknown"
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let query = decode_f32s(&mut reader)?;
    let k = reader.u64()?;
    let tier = reader.u8()?;
    let observation_phase = reader.u8()?;
    let public_schedule = decode_steps(&mut reader)?;
    reader.finish()?;
    Ok(IdentityInput {
        case_id,
        mutations,
        query,
        k,
        tier,
        observation_phase,
        public_schedule,
    })
}

fn decode_i27_observed(bytes: &[u8]) -> Result<I27Observed, String> {
    let mut reader = CanonicalReader::new(bytes, VectorCanonicalKind::I27Observed)?;
    let case_id = reader.u64()?;
    let rows = decode_identity_rows(&mut reader)?;
    let generation = reader.u64()?;
    let phase = reader.u8()?;
    let tier = reader.u8()?;
    let control_rows = decode_identity_rows(&mut reader)?;
    let control_generation = reader.u64()?;
    let retry_rows = decode_identity_rows(&mut reader)?;
    let retry_generation = reader.u64()?;
    let fault_status = decode_status(&mut reader)?;
    let retry_status = decode_status(&mut reader)?;
    reader.finish()?;
    Ok(I27Observed {
        case_id,
        rows,
        generation,
        phase,
        tier,
        control_rows,
        control_generation,
        retry_rows,
        retry_generation,
        fault_status,
        retry_status,
    })
}

/// Decodes and executes one I24-I27 checker from literal retained bytes.
///
/// No seed, current corpus generator, or production DTO is consulted. Both
/// canonical records must be complete, correctly versioned, and paired.
pub fn replay_canonical_comparison(
    input_bytes: &[u8],
    observed_bytes: &[u8],
) -> Result<VectorCanonicalReplay, String> {
    let (checker_id, case_id, first_difference) = match canonical_kind(input_bytes)? {
        VectorCanonicalKind::I24Input => {
            let input = decode_i24_input(input_bytes)?;
            let observed = decode_i24_observed(observed_bytes)?;
            let expected = expected_kernel(&input)?;
            (
                I24_CHECKER_ID,
                input.case_id,
                first_difference_i24(&expected, &observed),
            )
        }
        VectorCanonicalKind::I25Input => {
            let input = decode_i25_input(input_bytes)?;
            let observed = decode_i25_observed(observed_bytes)?;
            let expected = expected_quantization(&input);
            (
                I25_CHECKER_ID,
                input.case_id,
                first_difference_i25(&expected, &observed),
            )
        }
        VectorCanonicalKind::I26Input => {
            let input = decode_i26_input(input_bytes)?;
            let observed = decode_i26_observed(observed_bytes)?;
            let expected = expected_rescore(&input);
            (
                I26_CHECKER_ID,
                input.case_id,
                first_difference_i26(&expected, &observed),
            )
        }
        VectorCanonicalKind::I27Input => {
            let input = decode_i27_input(input_bytes)?;
            let observed = decode_i27_observed(observed_bytes)?;
            let expected = expected_identity(&input)?;
            (
                I27_CHECKER_ID,
                input.case_id,
                first_difference_i27(&expected, &observed),
            )
        }
        kind => {
            return Err(format!(
                "retained vector replay requires an input record, observed {kind:?}"
            ));
        }
    };
    Ok(VectorCanonicalReplay {
        checker_id,
        case_id,
        input_sha256: canonical_sha256(input_bytes),
        observed_sha256: canonical_sha256(observed_bytes),
        first_difference,
    })
}

fn first_difference(
    checker_id: &'static str,
    path: &'static str,
    expected: impl std::fmt::Debug,
    observed: impl std::fmt::Debug,
) -> Option<VectorFirstDifference> {
    Some(VectorFirstDifference {
        checker_id,
        path,
        expected: format!("{expected:?}"),
        observed: format!("{observed:?}"),
    })
}

#[must_use]
pub fn first_difference_i24(
    expected: &I24Expected,
    observed: &I24Observed,
) -> Option<VectorFirstDifference> {
    if check_i24(expected, observed).is_ok() {
        return None;
    }
    if expected.case_id != observed.case_id {
        return first_difference(
            I24_CHECKER_ID,
            "case_id",
            expected.case_id,
            observed.case_id,
        );
    }
    if expected.backend != observed.backend {
        return first_difference(
            I24_CHECKER_ID,
            "backend",
            expected.backend,
            observed.backend,
        );
    }
    if expected.kernel != observed.kernel {
        return first_difference(I24_CHECKER_ID, "kernel", expected.kernel, observed.kernel);
    }
    if expected.selected_for_store != observed.selected_for_store {
        return first_difference(
            I24_CHECKER_ID,
            "selected_for_store",
            expected.selected_for_store,
            observed.selected_for_store,
        );
    }
    if expected.work_items != observed.work_items {
        return first_difference(
            I24_CHECKER_ID,
            "work_items",
            expected.work_items,
            observed.work_items,
        );
    }
    first_difference(I24_CHECKER_ID, "value", &expected.exact, &observed.value)
}

#[must_use]
pub fn first_difference_i25(
    expected: &I25Expected,
    observed: &I25Observed,
) -> Option<VectorFirstDifference> {
    if check_i25(expected, observed).is_ok() {
        return None;
    }
    if expected.input.case_id != observed.case_id {
        return first_difference(
            I25_CHECKER_ID,
            "case_id",
            expected.input.case_id,
            observed.case_id,
        );
    }
    if expected.result != observed.result {
        return first_difference(I25_CHECKER_ID, "result", &expected.result, &observed.result);
    }
    first_difference(
        I25_CHECKER_ID,
        "store",
        "independently derived Store facts",
        &observed.store,
    )
}

#[must_use]
pub fn first_difference_i26(
    expected: &I26Expected,
    observed: &I26Observed,
) -> Option<VectorFirstDifference> {
    if check_i26(expected, observed).is_ok() {
        return None;
    }
    if expected.case_id != observed.case_id {
        return first_difference(
            I26_CHECKER_ID,
            "case_id",
            expected.case_id,
            observed.case_id,
        );
    }
    if expected.status != observed.primitive_status {
        return first_difference(
            I26_CHECKER_ID,
            "primitive_status",
            &expected.status,
            &observed.primitive_status,
        );
    }
    if !rescore_hits_match(expected, &observed.primitive_hits) {
        return first_difference(
            I26_CHECKER_ID,
            "primitive_hits",
            &expected.hits,
            &observed.primitive_hits,
        );
    }
    let expected_counts = [
        expected.candidates_rescored,
        expected.coarse_bytes,
        expected.rescore_bytes,
        expected.total_bytes,
    ];
    let count_paths = [
        "primitive_counts.candidates_rescored",
        "primitive_counts.coarse_bytes",
        "primitive_counts.rescore_bytes",
        "primitive_counts.total_bytes",
    ];
    for (index, path) in count_paths.into_iter().enumerate() {
        if expected_counts[index] != observed.primitive_counts[index] {
            return first_difference(
                I26_CHECKER_ID,
                path,
                expected_counts[index],
                observed.primitive_counts[index],
            );
        }
    }
    if expected.store_tier.unwrap_or(0) != observed.tier {
        return first_difference(
            I26_CHECKER_ID,
            "tier",
            expected.store_tier.unwrap_or(0),
            observed.tier,
        );
    }
    if expected.store_hits != observed.store_hits {
        return first_difference(
            I26_CHECKER_ID,
            "store_hits",
            &expected.store_hits,
            &observed.store_hits,
        );
    }
    first_difference(
        I26_CHECKER_ID,
        "exact_rescore",
        expected.store_exact_rescore,
        observed.exact_rescore,
    )
}

#[must_use]
pub fn first_difference_i27(
    expected: &I27Expected,
    observed: &I27Observed,
) -> Option<VectorFirstDifference> {
    if check_i27(expected, observed).is_ok() {
        return None;
    }
    if expected.case_id != observed.case_id {
        return first_difference(
            I27_CHECKER_ID,
            "case_id",
            expected.case_id,
            observed.case_id,
        );
    }
    if expected.generation != observed.generation {
        return first_difference(
            I27_CHECKER_ID,
            "generation",
            expected.generation,
            observed.generation,
        );
    }
    if expected.phase != observed.phase {
        return first_difference(I27_CHECKER_ID, "phase", expected.phase, observed.phase);
    }
    if expected.tier != observed.tier {
        return first_difference(I27_CHECKER_ID, "tier", expected.tier, observed.tier);
    }
    if expected.visible.len() != observed.rows.len()
        || expected
            .visible
            .iter()
            .zip(&observed.rows)
            .any(|(expected, observed)| {
                observed.document != Some(expected.document) || observed.row != expected.row
            })
    {
        return first_difference(I27_CHECKER_ID, "rows", &expected.visible, &observed.rows);
    }
    if observed.rows != observed.control_rows {
        return first_difference(
            I27_CHECKER_ID,
            "control_rows",
            &observed.rows,
            &observed.control_rows,
        );
    }
    if observed.rows != observed.retry_rows {
        return first_difference(
            I27_CHECKER_ID,
            "retry_rows",
            &observed.rows,
            &observed.retry_rows,
        );
    }
    if expected.generation != observed.control_generation {
        return first_difference(
            I27_CHECKER_ID,
            "control_generation",
            expected.generation,
            observed.control_generation,
        );
    }
    if expected.generation != observed.retry_generation {
        return first_difference(
            I27_CHECKER_ID,
            "retry_generation",
            expected.generation,
            observed.retry_generation,
        );
    }
    first_difference(
        I27_CHECKER_ID,
        "fault_or_retry_status",
        PrimitiveStatus::Ok,
        (&observed.fault_status, &observed.retry_status),
    )
}

/// Computes the exact SHA-256 used by vector canonical attestations.
///
/// The shared verifier uses this function to recompute retained canonical
/// bytes; it must not trust a digest copied from the record under audit.
#[must_use]
pub fn canonical_sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());
    let mut state = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let temporary1 = h
                .wrapping_add(sigma1)
                .wrapping_add(choose)
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary2 = sigma0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary1);
            d = c;
            c = b;
            b = a;
            a = temporary1.wrapping_add(temporary2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut output = [0_u8; 32];
    for (chunk, value) in output.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    output
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct F32(pub u32);

impl F32 {
    #[must_use]
    pub const fn from_float(value: f32) -> Self {
        Self(value.to_bits())
    }

    #[must_use]
    pub const fn to_float(self) -> f32 {
        f32::from_bits(self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct F64(pub u64);

impl F64 {
    #[must_use]
    pub const fn from_float(value: f64) -> Self {
        Self(value.to_bits())
    }

    #[must_use]
    pub const fn to_float(self) -> f64 {
        f64::from_bits(self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PrimitiveDocument {
    pub doc_id_be: [u8; 16],
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PrimitiveSource {
    Active,
    Sealed([u8; 16]),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PrimitiveRow {
    pub source: PrimitiveSource,
    pub local_row: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrimitiveStatus {
    Ok,
    EmptyVector,
    DimensionTooLarge { actual: u64, maximum: u64 },
    NonFinite { index: u64 },
    OutputLength { expected: u64, actual: u64 },
    CodeLength { expected: u64, actual: u64 },
    NonZeroPadding { byte: u8, mask: u8 },
    CandidateRowCount { expected: u64, actual: u64 },
    CandidateRowOutOfRange { row: u64, rows: u64 },
    NonFiniteScore { row: u64 },
    Cancelled { partial: bool },
    AllocationFailed { component: String, needed: u64 },
    SegmentGeometry { detail: String },
}

/// One public Store step whose generation semantics are independently fixed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicStoreStep {
    IngestAccepted,
    IngestRejected,
    Seal,
    PublishPreparedSegment,
    DeleteAccepted,
    Reopen,
    Search,
}

fn public_generation(schedule: &[PublicStoreStep], before: u64) -> Result<u64, String> {
    schedule.iter().try_fold(before, |generation, step| {
        let delta = match step {
            PublicStoreStep::IngestAccepted
            | PublicStoreStep::PublishPreparedSegment
            | PublicStoreStep::DeleteAccepted => 1,
            // Public sealing acknowledges one new immutable snapshot
            // generation; rotating the active buffers is not a second
            // externally visible generation.
            PublicStoreStep::Seal => 1,
            PublicStoreStep::IngestRejected | PublicStoreStep::Reopen | PublicStoreStep::Search => {
                0
            }
        };
        generation
            .checked_add(delta)
            .ok_or_else(|| "public Store generation overflow".to_owned())
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelId {
    DotI8,
    HammingU1,
    DotF32,
    DotF16,
    DotI8Batch,
    HammingU1Batch,
    DotBit4,
    DotBit4Prepared,
    DotBit4Batch,
    ScoreBit4PreparedBatch,
    ScoreBit4Ptrs,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BackendId {
    Scalar,
    NeonWiden,
    NeonDotprodU4,
    NeonI8mm,
    NeonDotprodU2,
    NeonDotprodU6,
    NeonDotprodU8,
    NeonDotprodU4Prefetch,
    Avx2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelInput {
    pub case_id: u64,
    pub backend: BackendId,
    pub selected_for_store: bool,
    pub work_items: u64,
    pub kernel: KernelId,
    pub dimension: u64,
    pub signed_a: Vec<i8>,
    pub signed_b: Vec<i8>,
    pub bytes_a: Vec<u8>,
    pub bytes_b: Vec<u8>,
    pub f32_a: Vec<F32>,
    pub f32_b: Vec<F32>,
    pub f16_a: Vec<u16>,
    pub f16_b: Vec<u16>,
    pub row_bytes: u64,
    pub batch_rows: u64,
    pub pointer_order: Vec<u8>,
    pub query_sum: i32,
    pub query_scale_half: F64,
    pub bit4_factors: Vec<[F32; 3]>,
}

impl KernelInput {
    /// Deliberate element offset used by the production observation adapter.
    #[must_use]
    pub const fn input_offset(&self) -> usize {
        ((self.case_id >> 63) & 1) as usize
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelValue {
    S32(i32),
    U32(u32),
    F32(F32),
    S32s(Vec<i32>),
    U32s(Vec<u32>),
    F32s(Vec<F32>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I24Expected {
    pub case_id: u64,
    pub backend: BackendId,
    pub kernel: KernelId,
    pub selected_for_store: bool,
    pub work_items: u64,
    pub exact: KernelValue,
    pub reference: Option<F64>,
    pub magnitude: Option<F64>,
    pub tolerance: Option<F64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I24Observed {
    pub case_id: u64,
    pub backend: BackendId,
    pub kernel: KernelId,
    pub value: KernelValue,
    pub selected_for_store: bool,
    pub work_items: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuantScheme {
    Bit4,
    Int8,
}

/// Public Store fixture semantics paired with one codec case.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuantStoreInput {
    pub generation_before: u64,
    pub schedule: Vec<PublicStoreStep>,
    pub document: Option<PrimitiveDocument>,
    pub document_visible: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuantInput {
    pub case_id: u64,
    pub scheme: QuantScheme,
    pub row: Vec<F32>,
    pub query: Vec<F32>,
    pub query_seed: u64,
    pub output_len: u64,
    pub code_len: u64,
    pub sentinel: u8,
    pub store: QuantStoreInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuantSuccess {
    pub code_bytes: Vec<u8>,
    pub factor_bits: Vec<F32>,
    pub query_code_bytes: Vec<u8>,
    pub query_code_sum: i32,
    pub query_scale: F64,
    pub reconstruction: Vec<F32>,
    pub estimate: F32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuantResult {
    pub status: PrimitiveStatus,
    pub output_after: Vec<u8>,
    pub success: Option<QuantSuccess>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I25Expected {
    pub input: QuantInput,
    pub result: QuantResult,
    pub l2_error: F64,
    pub l2_bound: F64,
    pub estimate_reference: F64,
    pub estimate_error: F64,
    pub estimate_bound: F64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I25Observed {
    pub case_id: u64,
    pub result: QuantResult,
    pub store: QuantStoreFacts,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuantStoreFacts {
    pub ingest_status: PrimitiveStatus,
    pub scan_status: PrimitiveStatus,
    pub generation_before: u64,
    pub generation_after: u64,
    pub document_visible: bool,
    pub persisted_code_bytes: Vec<u8>,
    pub persisted_factor_bits: Vec<F32>,
}

/// Literal vector-code and factor payloads parsed from one persisted segment.
///
/// This DTO intentionally contains only primitive layout facts. The parser
/// below has no dependency on the production segment reader or quantizers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedQuantization {
    pub segment_id: [u8; 16],
    pub scheme: QuantScheme,
    pub dims: u32,
    pub row_count: u32,
    pub code_stride: u32,
    pub factor_stride: u16,
    pub code_bytes: Vec<u8>,
    pub factor_bits: Vec<F32>,
}

#[derive(Clone, Copy)]
struct LiteralRegion {
    offset: usize,
    length: usize,
}

/// Parses the frozen segment/vector layout using only literal offsets and
/// little-endian primitives.
///
/// Persisted checksums remain a durability concern. This parser establishes
/// the independent byte/factor observation used by I25 after the production
/// reader has accepted and published the same immutable file.
pub fn parse_persisted_quantization(bytes: &[u8]) -> Result<PersistedQuantization, String> {
    const FILE_HEADER_LEN: usize = 32;
    const SEGMENT_PREFIX_LEN: usize = 32;
    const REGION_ENTRY_LEN: usize = 32;
    const VECTOR_HEADER_LEN: usize = 32;
    const SEGMENT_FAMILY: u16 = 2;
    const VECTOR_CODES_KIND: u16 = 3;
    const VECTOR_FACTORS_KIND: u16 = 4;

    if literal_slice(bytes, 0, 8)? != b"ZEPEMBED" {
        return Err("persisted vector segment has the wrong magic".to_owned());
    }
    if literal_u16(bytes, 8)? != SEGMENT_FAMILY {
        return Err("persisted vector artifact is not a segment".to_owned());
    }
    if literal_u16(bytes, 10)? != 1 || literal_u32(bytes, 12)? != 0 {
        return Err("persisted vector segment has unsupported common header fields".to_owned());
    }
    let header_length = literal_usize(literal_u64(bytes, 16)?, "segment header length")?;
    let file_length = literal_usize(literal_u64(bytes, 24)?, "segment file length")?;
    if file_length != bytes.len() {
        return Err(format!(
            "persisted vector segment declares {file_length} bytes, got {}",
            bytes.len()
        ));
    }
    let segment_id: [u8; 16] = literal_slice(bytes, FILE_HEADER_LEN, 16)?
        .try_into()
        .map_err(|_| "persisted vector segment ID width changed".to_owned())?;
    let row_count = literal_u32(bytes, FILE_HEADER_LEN + 16)?;
    let region_count = usize::from(literal_u16(bytes, FILE_HEADER_LEN + 20)?);
    if literal_u16(bytes, FILE_HEADER_LEN + 22)? != 0 {
        return Err("persisted vector segment prefix reserved field is non-zero".to_owned());
    }
    let scheme_id = literal_u16(bytes, FILE_HEADER_LEN + 24)?;
    let scheme = literal_quant_scheme(scheme_id)?;
    if literal_u16(bytes, FILE_HEADER_LEN + 26)? != 0 {
        return Err("persisted vector segment scheme padding is non-zero".to_owned());
    }
    let dims = literal_u32(bytes, FILE_HEADER_LEN + 28)?;
    let expected_header = FILE_HEADER_LEN
        .checked_add(SEGMENT_PREFIX_LEN)
        .and_then(|value| value.checked_add(region_count.checked_mul(REGION_ENTRY_LEN)?))
        .and_then(|value| value.checked_add(8))
        .ok_or_else(|| "persisted vector header length overflow".to_owned())?;
    if header_length != expected_header || header_length > bytes.len() {
        return Err(format!(
            "persisted vector header length {header_length} does not match directory {expected_header}"
        ));
    }

    let directory_start = FILE_HEADER_LEN + SEGMENT_PREFIX_LEN;
    let mut codes = None;
    let mut factors = None;
    for position in 0..region_count {
        let offset = directory_start
            .checked_add(
                position
                    .checked_mul(REGION_ENTRY_LEN)
                    .ok_or_else(|| "persisted vector directory offset overflow".to_owned())?,
            )
            .ok_or_else(|| "persisted vector directory offset overflow".to_owned())?;
        let kind = literal_u16(bytes, offset)?;
        if literal_u16(bytes, offset + 2)? != 1 || literal_u32(bytes, offset + 4)? != 0 {
            return Err(format!(
                "persisted vector region {kind} has unsupported directory fields"
            ));
        }
        let region = LiteralRegion {
            offset: literal_usize(literal_u64(bytes, offset + 8)?, "region offset")?,
            length: literal_usize(literal_u64(bytes, offset + 16)?, "region length")?,
        };
        let _ = literal_slice(bytes, region.offset, region.length)?;
        match kind {
            VECTOR_CODES_KIND if codes.replace(region).is_some() => {
                return Err("persisted vector segment has duplicate code regions".to_owned());
            }
            VECTOR_FACTORS_KIND if factors.replace(region).is_some() => {
                return Err("persisted vector segment has duplicate factor regions".to_owned());
            }
            _ => {}
        }
    }
    let codes = codes.ok_or_else(|| "persisted vector segment has no code region".to_owned())?;
    let factors =
        factors.ok_or_else(|| "persisted vector segment has no factor region".to_owned())?;
    let code_header = literal_vector_header(
        literal_slice(bytes, codes.offset, codes.length)?,
        scheme_id,
        dims,
        row_count,
    )?;
    let factor_header = literal_vector_header(
        literal_slice(bytes, factors.offset, factors.length)?,
        scheme_id,
        dims,
        row_count,
    )?;
    if code_header != factor_header {
        return Err("persisted vector code/factor headers disagree".to_owned());
    }
    let (code_stride, factor_stride) = code_header;
    let expected_code_stride = match scheme {
        QuantScheme::Bit4 => dims.div_ceil(2),
        QuantScheme::Int8 => dims,
    };
    let expected_factor_stride = match scheme {
        QuantScheme::Bit4 => 12_u16,
        QuantScheme::Int8 => 8_u16,
    };
    if code_stride != expected_code_stride || factor_stride != expected_factor_stride {
        return Err(format!(
            "persisted vector strides {code_stride}/{factor_stride} do not match scheme {scheme:?} geometry {expected_code_stride}/{expected_factor_stride}"
        ));
    }
    let code_payload_len = usize::try_from(code_stride)
        .ok()
        .and_then(|stride| stride.checked_mul(row_count as usize))
        .ok_or_else(|| "persisted vector code payload length overflow".to_owned())?;
    let factor_payload_len = usize::from(factor_stride)
        .checked_mul(row_count as usize)
        .ok_or_else(|| "persisted vector factor payload length overflow".to_owned())?;
    if codes.length != VECTOR_HEADER_LEN + code_payload_len
        || factors.length != VECTOR_HEADER_LEN + factor_payload_len
    {
        return Err("persisted vector payload lengths do not match their headers".to_owned());
    }
    let code_bytes =
        literal_slice(bytes, codes.offset + VECTOR_HEADER_LEN, code_payload_len)?.to_vec();
    let factor_bytes = literal_slice(
        bytes,
        factors.offset + VECTOR_HEADER_LEN,
        factor_payload_len,
    )?;
    let factor_bits = factor_bytes
        .chunks_exact(4)
        .map(|chunk| {
            <[u8; 4]>::try_from(chunk)
                .map(u32::from_le_bytes)
                .map(F32)
                .map_err(|_| "persisted vector factor field width changed".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PersistedQuantization {
        segment_id,
        scheme,
        dims,
        row_count,
        code_stride,
        factor_stride,
        code_bytes,
        factor_bits,
    })
}

fn literal_quant_scheme(scheme: u16) -> Result<QuantScheme, String> {
    match scheme {
        4 => Ok(QuantScheme::Bit4),
        2 => Ok(QuantScheme::Int8),
        other => Err(format!(
            "persisted vector segment has unsupported scheme {other}"
        )),
    }
}

fn literal_vector_header(
    region: &[u8],
    scheme: u16,
    dims: u32,
    row_count: u32,
) -> Result<(u32, u16), String> {
    if literal_u16(region, 0)? != scheme
        || literal_u16(region, 2)? != 0
        || literal_u32(region, 4)? != dims
        || literal_u16(region, 14)? != 0
        || literal_u64(region, 16)? != 0
        || literal_u32(region, 24)? != row_count
        || literal_u32(region, 28)? != 0
    {
        return Err("persisted vector region header does not match segment geometry".to_owned());
    }
    Ok((literal_u32(region, 8)?, literal_u16(region, 12)?))
}

fn literal_slice(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8], String> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| "persisted vector byte range overflow".to_owned())?;
    bytes
        .get(offset..end)
        .ok_or_else(|| format!("persisted vector byte range {offset}..{end} is out of bounds"))
}

fn literal_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    literal_slice(bytes, offset, 2)?
        .try_into()
        .map(u16::from_le_bytes)
        .map_err(|_| "persisted vector u16 width changed".to_owned())
}

fn literal_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    literal_slice(bytes, offset, 4)?
        .try_into()
        .map(u32::from_le_bytes)
        .map_err(|_| "persisted vector u32 width changed".to_owned())
}

fn literal_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    literal_slice(bytes, offset, 8)?
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| "persisted vector u64 width changed".to_owned())
}

fn literal_usize(value: u64, field: &str) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| format!("persisted vector {field} exceeds usize"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RescoreMetric {
    InnerProduct,
    SquaredL2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateMode {
    Dense { coarse: Vec<F32>, oversample: u64 },
    Retained { rows: Vec<u32>, coarse: Vec<F32> },
}

/// Public exact-rescore identity/tier semantics for one primitive case.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RescoreStoreInput {
    pub source: PrimitiveSource,
    pub documents_by_row: Vec<Option<PrimitiveDocument>>,
    pub tier: u8,
    pub exact_rescore: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RescoreInput {
    pub case_id: u64,
    pub metric: RescoreMetric,
    pub query: Vec<F32>,
    pub rows_row_major: Vec<F32>,
    pub dimension: u64,
    pub k: u64,
    pub candidates: CandidateMode,
    pub coarse_rows_touched: u64,
    pub coarse_bytes_per_row: u64,
    pub store: Option<RescoreStoreInput>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrimitiveRescoreHit {
    pub row: u32,
    pub score: F64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I26Expected {
    pub case_id: u64,
    pub metric: RescoreMetric,
    pub status: PrimitiveStatus,
    pub hits: Vec<PrimitiveRescoreHit>,
    pub score_tolerances: Vec<F64>,
    pub candidates_rescored: u64,
    pub coarse_bytes: u64,
    pub rescore_bytes: u64,
    pub total_bytes: u64,
    pub store_hits: Vec<StoreRescoreHit>,
    pub store_tier: Option<u8>,
    pub store_exact_rescore: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreRescoreHit {
    pub row: PrimitiveRow,
    pub document: Option<PrimitiveDocument>,
    pub score: F32,
    pub exact_score: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I26Observed {
    pub case_id: u64,
    pub primitive_status: PrimitiveStatus,
    pub primitive_hits: Vec<PrimitiveRescoreHit>,
    pub primitive_counts: [u64; 4],
    pub tier: u8,
    pub store_hits: Vec<StoreRescoreHit>,
    pub exact_rescore: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityMutation {
    Ingest(PrimitiveDocument),
    Seal([u8; 16]),
    Reopen,
    Replace(PrimitiveDocument),
    Delete { doc_id_be: [u8; 16], revision: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityInput {
    pub case_id: u64,
    pub mutations: Vec<IdentityMutation>,
    pub query: Vec<F32>,
    pub k: u64,
    pub tier: u8,
    pub observation_phase: u8,
    pub public_schedule: Vec<PublicStoreStep>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityExpectedRow {
    pub document: PrimitiveDocument,
    pub row: PrimitiveRow,
    pub phase: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityObservedRow {
    pub row: PrimitiveRow,
    pub document: Option<PrimitiveDocument>,
    pub score: F32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I27Expected {
    pub case_id: u64,
    pub visible: Vec<IdentityExpectedRow>,
    pub forbidden: Vec<PrimitiveDocument>,
    pub generation: u64,
    pub phase: u8,
    pub tier: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I27Observed {
    pub case_id: u64,
    pub rows: Vec<IdentityObservedRow>,
    pub generation: u64,
    pub phase: u8,
    pub tier: u8,
    pub control_rows: Vec<IdentityObservedRow>,
    pub control_generation: u64,
    pub retry_rows: Vec<IdentityObservedRow>,
    pub retry_generation: u64,
    pub fault_status: PrimitiveStatus,
    pub retry_status: PrimitiveStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckerFailure {
    pub checker_id: &'static str,
    pub case_id: u64,
    pub detail: String,
}

impl std::fmt::Display for CheckerFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} case={} {}",
            self.checker_id, self.case_id, self.detail
        )
    }
}

impl std::error::Error for CheckerFailure {}

fn mismatch(checker_id: &'static str, case_id: u64, detail: String) -> CheckerFailure {
    CheckerFailure {
        checker_id,
        case_id,
        detail,
    }
}

pub fn check_i24(expected: &I24Expected, observed: &I24Observed) -> Result<(), CheckerFailure> {
    if expected.case_id != observed.case_id
        || expected.backend != observed.backend
        || expected.kernel != observed.kernel
        || expected.selected_for_store != observed.selected_for_store
        || expected.work_items != observed.work_items
    {
        return Err(mismatch(
            I24_CHECKER_ID,
            expected.case_id,
            format!(
                "identity/work mismatch: expected backend={:?} kernel={:?} selected_for_store={} work_items={}, observed case={} backend={:?} kernel={:?} selected_for_store={} work_items={}",
                expected.backend,
                expected.kernel,
                expected.selected_for_store,
                expected.work_items,
                observed.case_id,
                observed.backend,
                observed.kernel,
                observed.selected_for_store,
                observed.work_items
            ),
        ));
    }
    match (expected.reference, expected.magnitude, &observed.value) {
        (Some(reference), Some(magnitude), KernelValue::F32(value)) => check_float_kernel(
            expected,
            reference.to_float(),
            magnitude.to_float(),
            value.to_float(),
            observed,
        ),
        _ if expected.exact == observed.value => Ok(()),
        _ => Err(mismatch(
            I24_CHECKER_ID,
            expected.case_id,
            format!(
                "backend={:?} expected={:?} observed={:?}",
                observed.backend, expected.exact, observed.value
            ),
        )),
    }
}

fn check_float_kernel(
    expected: &I24Expected,
    reference: f64,
    magnitude: f64,
    observed_value: f32,
    observed: &I24Observed,
) -> Result<(), CheckerFailure> {
    let observed_f64 = f64::from(observed_value);
    let matches_special = (reference.is_nan() && observed_f64.is_nan())
        || (reference.is_infinite()
            && observed_f64.is_infinite()
            && reference.is_sign_negative() == observed_f64.is_sign_negative());
    let tolerance = expected
        .tolerance
        .map_or_else(|| 1.0e-5 * magnitude.max(1.0), F64::to_float);
    let within_bound = reference.is_finite()
        && observed_f64.is_finite()
        && (observed_f64 - reference).abs() <= tolerance;
    let within_f16_ulp = expected.kernel == KernelId::DotF16
        && expected.exact == KernelValue::F32(F32::from_float(reference as f32))
        && ordered_f32_distance(observed_value, reference as f32) <= 1;
    if matches_special || within_bound || within_f16_ulp {
        return Ok(());
    }
    Err(mismatch(
        I24_CHECKER_ID,
        expected.case_id,
        format!(
            "backend={:?} expected_reference={reference:?} tolerance={tolerance:?} observed={observed_f64:?}",
            observed.backend
        ),
    ))
}

pub fn check_i25(expected: &I25Expected, observed: &I25Observed) -> Result<(), CheckerFailure> {
    if expected.input.case_id != observed.case_id || expected.result != observed.result {
        return Err(mismatch(
            I25_CHECKER_ID,
            expected.input.case_id,
            format!(
                "expected={:?} observed_case={} observed={:?}",
                expected.result, observed.case_id, observed.result
            ),
        ));
    }
    if expected.l2_error.to_float() > expected.l2_bound.to_float() {
        return Err(mismatch(
            I25_CHECKER_ID,
            expected.input.case_id,
            format!(
                "reconstruction error {} exceeds bound {}",
                expected.l2_error.to_float(),
                expected.l2_bound.to_float()
            ),
        ));
    }
    if expected.estimate_error.to_float() > expected.estimate_bound.to_float() {
        return Err(mismatch(
            I25_CHECKER_ID,
            expected.input.case_id,
            format!(
                "estimate error {} exceeds bound {}",
                expected.estimate_error.to_float(),
                expected.estimate_bound.to_float()
            ),
        ));
    }
    check_i25_store_facts(expected, &observed.store)
}

fn check_i25_store_facts(
    expected: &I25Expected,
    observed: &QuantStoreFacts,
) -> Result<(), CheckerFailure> {
    if expected.input.store.schedule.is_empty() {
        if observed.ingest_status == PrimitiveStatus::Ok
            && observed.scan_status == PrimitiveStatus::Ok
            && observed.generation_before == 0
            && observed.generation_after == 0
            && !observed.document_visible
            && observed.persisted_code_bytes.is_empty()
            && observed.persisted_factor_bits.is_empty()
        {
            return Ok(());
        }
        return Err(mismatch(
            I25_CHECKER_ID,
            expected.input.case_id,
            format!("primitive-only case fabricated Store facts: {observed:?}"),
        ));
    }
    let expected_generation = public_generation(
        &expected.input.store.schedule,
        expected.input.store.generation_before,
    )
    .map_err(|detail| mismatch(I25_CHECKER_ID, expected.input.case_id, detail))?;
    let row = expected
        .input
        .row
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let row_status = validate_quant_vector(&row);
    if let Err(status) = row_status {
        if observed.ingest_status == status
            && observed.scan_status == PrimitiveStatus::Ok
            && observed.generation_before == expected.input.store.generation_before
            && observed.generation_after == expected_generation
            && observed.document_visible == expected.input.store.document_visible
            && observed.persisted_code_bytes.is_empty()
            && observed.persisted_factor_bits.is_empty()
        {
            return Ok(());
        }
        return Err(mismatch(
            I25_CHECKER_ID,
            expected.input.case_id,
            format!("invalid-row Store atomicity mismatch: {observed:?}"),
        ));
    }

    let (code_bytes, factor_bits) = match expected.input.scheme {
        QuantScheme::Bit4 => {
            let (codes, factors, _) = encode_bit4(&row);
            (codes, factors.map(F32::from_float).to_vec())
        }
        QuantScheme::Int8 => {
            let (codes, scale, offset) = encode_int8(&row);
            (
                codes.into_iter().map(|code| code as u8).collect(),
                vec![F32::from_float(scale), F32::from_float(offset)],
            )
        }
    };
    if observed.ingest_status != PrimitiveStatus::Ok
        || observed.scan_status != PrimitiveStatus::Ok
        || observed.generation_before != expected.input.store.generation_before
        || observed.generation_after != expected_generation
        || observed.document_visible != expected.input.store.document_visible
        || observed.persisted_code_bytes != code_bytes
        || observed.persisted_factor_bits != factor_bits
    {
        return Err(mismatch(
            I25_CHECKER_ID,
            expected.input.case_id,
            format!(
                "valid-row Store/persistence mismatch: expected_codes={code_bytes:?} expected_factors={factor_bits:?} observed={observed:?}"
            ),
        ));
    }
    Ok(())
}

pub fn check_i26(expected: &I26Expected, observed: &I26Observed) -> Result<(), CheckerFailure> {
    let expected_counts = [
        expected.candidates_rescored,
        expected.coarse_bytes,
        expected.rescore_bytes,
        expected.total_bytes,
    ];
    let hits_match = rescore_hits_match(expected, &observed.primitive_hits);
    if expected.case_id != observed.case_id
        || expected.status != observed.primitive_status
        || !hits_match
        || expected_counts != observed.primitive_counts
    {
        return Err(mismatch(
            I26_CHECKER_ID,
            expected.case_id,
            format!(
                "primitive mismatch: expected_status={:?} observed_status={:?} expected_hits={:?} observed_hits={:?} expected_counts={expected_counts:?} observed_counts={:?}",
                expected.status,
                observed.primitive_status,
                expected.hits,
                observed.primitive_hits,
                observed.primitive_counts
            ),
        ));
    }
    let Some(store_tier) = expected.store_tier else {
        if observed.tier == 0 && observed.store_hits.is_empty() && !observed.exact_rescore {
            return Ok(());
        }
        return Err(mismatch(
            I26_CHECKER_ID,
            expected.case_id,
            format!(
                "primitive-only case fabricated Store evidence: tier={} hits={:?} exact_rescore={}",
                observed.tier, observed.store_hits, observed.exact_rescore
            ),
        ));
    };
    if observed.store_hits.is_empty() {
        return Err(mismatch(
            I26_CHECKER_ID,
            expected.case_id,
            "Store result omitted every expected rescored hit".to_owned(),
        ));
    }
    if observed.tier != store_tier {
        return Err(mismatch(
            I26_CHECKER_ID,
            expected.case_id,
            format!(
                "Store tier mismatch: expected={store_tier} observed={}",
                observed.tier
            ),
        ));
    }
    if expected.store_hits != observed.store_hits {
        return Err(mismatch(
            I26_CHECKER_ID,
            expected.case_id,
            format!(
                "Store membership/score mismatch: expected={:?} observed={:?}",
                expected.store_hits, observed.store_hits
            ),
        ));
    }
    if observed.exact_rescore != expected.store_exact_rescore {
        return Err(mismatch(
            I26_CHECKER_ID,
            expected.case_id,
            format!(
                "tier={} expected exact_rescore={} observed={}",
                observed.tier, expected.store_exact_rescore, observed.exact_rescore
            ),
        ));
    }
    if observed.store_hits.iter().any(|hit| {
        hit.exact_score != expected.store_exact_rescore || !hit.score.to_float().is_finite()
    }) {
        return Err(mismatch(
            I26_CHECKER_ID,
            expected.case_id,
            "Store result contained a non-exact or non-finite hit".to_owned(),
        ));
    }
    if !store_hits_best_first(&observed.store_hits) {
        return Err(mismatch(
            I26_CHECKER_ID,
            expected.case_id,
            format!(
                "Store document tie order is wrong: {:?}",
                observed.store_hits
            ),
        ));
    }
    Ok(())
}

fn rescore_hits_match(expected: &I26Expected, observed: &[PrimitiveRescoreHit]) -> bool {
    if expected.hits.len() != observed.len()
        || expected.score_tolerances.len() != expected.hits.len()
    {
        return false;
    }
    let metric = expected.metric;
    expected
        .hits
        .iter()
        .zip(&expected.score_tolerances)
        .zip(observed)
        .all(|((expected, tolerance), observed)| {
            expected.row == observed.row
                && match metric {
                    RescoreMetric::SquaredL2 => expected.score == observed.score,
                    RescoreMetric::InnerProduct => {
                        (expected.score.to_float() - observed.score.to_float()).abs()
                            <= tolerance.to_float()
                    }
                }
        })
}

pub fn check_i27(expected: &I27Expected, observed: &I27Observed) -> Result<(), CheckerFailure> {
    if expected.case_id != observed.case_id
        || expected.generation != observed.generation
        || expected.phase != observed.phase
        || expected.tier != observed.tier
    {
        return Err(mismatch(
            I27_CHECKER_ID,
            expected.case_id,
            format!(
                "phase/tier/generation mismatch: expected phase={} tier={} generation={}, observed case={} phase={} tier={} generation={}",
                expected.phase,
                expected.tier,
                expected.generation,
                observed.case_id,
                observed.phase,
                observed.tier,
                observed.generation
            ),
        ));
    }
    check_identity_rows(expected, &observed.rows, "fault-free")?;
    check_identity_rows(expected, &observed.control_rows, "same-seed control")?;
    check_identity_rows(expected, &observed.retry_rows, "post-clear retry")?;
    if observed.rows != observed.control_rows || observed.rows != observed.retry_rows {
        return Err(mismatch(
            I27_CHECKER_ID,
            expected.case_id,
            format!(
                "same-fixture order/score bits differ: clean={:?} control={:?} retry={:?}",
                observed.rows, observed.control_rows, observed.retry_rows
            ),
        ));
    }
    if observed.control_generation != expected.generation
        || observed.retry_generation != expected.generation
        || observed.retry_status != PrimitiveStatus::Ok
        || !matches!(
            observed.fault_status,
            PrimitiveStatus::Ok
                | PrimitiveStatus::Cancelled { partial: false }
                | PrimitiveStatus::AllocationFailed { .. }
                | PrimitiveStatus::SegmentGeometry { .. }
                | PrimitiveStatus::NonFiniteScore { .. }
                | PrimitiveStatus::NonZeroPadding { .. }
        )
    {
        return Err(mismatch(
            I27_CHECKER_ID,
            expected.case_id,
            format!(
                "control/retry mismatch: fault={:?} retry={:?} control_generation={} retry_generation={}",
                observed.fault_status,
                observed.retry_status,
                observed.control_generation,
                observed.retry_generation
            ),
        ));
    }
    Ok(())
}

fn check_identity_rows(
    expected: &I27Expected,
    observed: &[IdentityObservedRow],
    label: &str,
) -> Result<(), CheckerFailure> {
    let exact_order = expected.visible.len() == observed.len()
        && expected
            .visible
            .iter()
            .zip(observed)
            .all(|(expected, observed)| {
                observed.document == Some(expected.document)
                    && observed.row == expected.row
                    && observed.score.to_float().is_finite()
            });
    if !exact_order {
        return Err(mismatch(
            I27_CHECKER_ID,
            expected.case_id,
            format!(
                "{label}: expected ordered rows={:?} observed rows={observed:?}",
                expected.visible
            ),
        ));
    }
    if observed.iter().any(|row| {
        row.document
            .is_some_and(|document| expected.forbidden.contains(&document))
    }) {
        return Err(mismatch(
            I27_CHECKER_ID,
            expected.case_id,
            "a deleted or superseded version remained visible".to_owned(),
        ));
    }
    Ok(())
}

fn store_hits_best_first(hits: &[StoreRescoreHit]) -> bool {
    hits.windows(2).all(|pair| {
        let left = &pair[0];
        let right = &pair[1];
        let score_order = right.score.to_float().total_cmp(&left.score.to_float());
        if score_order.is_lt() {
            return true;
        }
        if score_order.is_gt() {
            return false;
        }
        match (left.document, right.document) {
            (Some(left_doc), Some(right_doc)) => {
                left_doc.doc_id_be < right_doc.doc_id_be
                    || (left_doc.doc_id_be == right_doc.doc_id_be
                        && (left_doc.revision > right_doc.revision
                            || (left_doc.revision == right_doc.revision && left.row <= right.row)))
            }
            (None, None) => left.row <= right.row,
            (Some(_), None) => true,
            (None, Some(_)) => false,
        }
    })
}

fn ordered_f32_distance(left: f32, right: f32) -> u32 {
    fn ordered(value: f32) -> i32 {
        let bits = value.to_bits() as i32;
        if bits < 0 { i32::MIN - bits } else { bits }
    }
    ordered(left).abs_diff(ordered(right))
}

pub fn expected_kernel(input: &KernelInput) -> Result<I24Expected, String> {
    let dimension = usize::try_from(input.dimension).map_err(|_| "dimension overflow")?;
    if dimension > MAX_I8_DIMENSION {
        return Err("dimension exceeds the exact i8 accumulator contract".to_owned());
    }
    let (exact, reference, magnitude) = match input.kernel {
        KernelId::DotI8 => (
            KernelValue::S32(checked_i8_dot(&input.signed_a, &input.signed_b)?),
            None,
            None,
        ),
        KernelId::HammingU1 => (
            KernelValue::U32(literal_hamming(&input.bytes_a, &input.bytes_b)?),
            None,
            None,
        ),
        KernelId::DotF32 => {
            let (reference, magnitude) = reference_f32_dot(&input.f32_a, &input.f32_b)?;
            (
                KernelValue::F32(F32::from_float(reference as f32)),
                Some(F64::from_float(reference)),
                Some(F64::from_float(magnitude)),
            )
        }
        KernelId::DotF16 => {
            if input.f16_a.len() != input.f16_b.len() {
                return Err("f16 length mismatch".to_owned());
            }
            let mut reference = 0.0_f64;
            let mut magnitude = 0.0_f64;
            for (&left, &right) in input.f16_a.iter().zip(&input.f16_b) {
                let product = f64::from(decode_f16(left)) * f64::from(decode_f16(right));
                reference += product;
                magnitude += product.abs();
            }
            (
                KernelValue::F32(F32::from_float(reference as f32)),
                Some(F64::from_float(reference)),
                Some(F64::from_float(magnitude)),
            )
        }
        KernelId::DotI8Batch => {
            require_len(&input.signed_a, dimension, "i8 batch query")?;
            let rows = usize::try_from(input.batch_rows).map_err(|_| "batch row overflow")?;
            require_len(
                &input.signed_b,
                dimension
                    .checked_mul(rows)
                    .ok_or("i8 batch shape overflow")?,
                "i8 batch rows",
            )?;
            let mut values = Vec::with_capacity(rows);
            if dimension == 0 {
                values.resize(rows, 0);
            } else {
                for row in input.signed_b.chunks_exact(dimension) {
                    values.push(checked_i8_dot(&input.signed_a, row)?);
                }
            }
            (KernelValue::S32s(values), None, None)
        }
        KernelId::HammingU1Batch => {
            require_len(&input.bytes_a, dimension, "Hamming batch query")?;
            let rows = usize::try_from(input.batch_rows).map_err(|_| "batch row overflow")?;
            require_len(
                &input.bytes_b,
                dimension
                    .checked_mul(rows)
                    .ok_or("Hamming batch shape overflow")?,
                "Hamming batch rows",
            )?;
            let mut values = Vec::with_capacity(rows);
            if dimension == 0 {
                values.resize(rows, 0);
            } else {
                for row in input.bytes_b.chunks_exact(dimension) {
                    values.push(literal_hamming(&input.bytes_a, row)?);
                }
            }
            (KernelValue::U32s(values), None, None)
        }
        KernelId::DotBit4 | KernelId::DotBit4Prepared => (
            KernelValue::S32(bit4_integer_dot(
                &input.signed_a,
                input.query_sum,
                &input.bytes_b,
                input.kernel == KernelId::DotBit4Prepared,
            )?),
            None,
            None,
        ),
        KernelId::DotBit4Batch => {
            require_len(&input.signed_a, dimension, "Bit4 batch query")?;
            let row_bytes = dimension.div_ceil(2);
            let rows = usize::try_from(input.batch_rows).map_err(|_| "batch row overflow")?;
            require_len(
                &input.bytes_b,
                row_bytes
                    .checked_mul(rows)
                    .ok_or("Bit4 batch shape overflow")?,
                "Bit4 batch rows",
            )?;
            let mut values = Vec::with_capacity(rows);
            if row_bytes == 0 {
                values.resize(rows, 0);
            } else {
                for row in input.bytes_b.chunks_exact(row_bytes) {
                    values.push(bit4_integer_dot(&input.signed_a, 0, row, false)?);
                }
            }
            (KernelValue::S32s(values), None, None)
        }
        KernelId::ScoreBit4PreparedBatch | KernelId::ScoreBit4Ptrs => {
            let row_bytes = dimension.div_ceil(2);
            let rows = usize::try_from(input.batch_rows).map_err(|_| "batch row overflow")?;
            require_len(
                &input.bytes_b,
                row_bytes
                    .checked_mul(rows)
                    .ok_or("Bit4 score shape overflow")?,
                "Bit4 score rows",
            )?;
            require_len(&input.bit4_factors, rows, "Bit4 factor rows")?;
            let mut values = Vec::with_capacity(rows);
            let order = if input.kernel == KernelId::ScoreBit4Ptrs {
                if input.pointer_order.len() != rows {
                    return Err("Bit4 pointer order length mismatch".to_owned());
                }
                input.pointer_order.clone()
            } else {
                (0..rows)
                    .map(|row| u8::try_from(row).map_err(|_| "batch row does not fit u8"))
                    .collect::<Result<Vec<_>, _>>()?
            };
            for pointer in order {
                let row = usize::from(pointer);
                let start = row.checked_mul(row_bytes).ok_or("Bit4 row overflow")?;
                let end = start.checked_add(row_bytes).ok_or("Bit4 row overflow")?;
                let codes = input
                    .bytes_b
                    .get(start..end)
                    .ok_or("Bit4 pointer row out of range")?;
                let factors = input
                    .bit4_factors
                    .get(row)
                    .ok_or("Bit4 pointer factor out of range")?;
                let scale = f64::from(factors[0].to_float());
                let correction = scale * f64::from(factors[2].to_float());
                let integer = bit4_integer_dot(&input.signed_a, input.query_sum, codes, true)?;
                values.push(F32::from_float(
                    (correction * input.query_scale_half.to_float() * f64::from(integer)) as f32,
                ));
            }
            (KernelValue::F32s(values), None, None)
        }
    };
    Ok(I24Expected {
        case_id: input.case_id,
        backend: input.backend,
        kernel: input.kernel,
        selected_for_store: input.selected_for_store,
        work_items: input.work_items,
        exact,
        reference,
        magnitude,
        tolerance: magnitude.map(|value| F64::from_float(1.0e-5 * value.to_float().max(1.0))),
    })
}

fn require_len<T>(values: &[T], expected: usize, name: &str) -> Result<(), String> {
    if values.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "{name} length mismatch: expected {expected}, got {}",
            values.len()
        ))
    }
}

fn checked_i8_dot(left: &[i8], right: &[i8]) -> Result<i32, String> {
    if left.len() != right.len() {
        return Err("i8 length mismatch".to_owned());
    }
    let mut sum = 0_i64;
    for (&left, &right) in left.iter().zip(right) {
        sum = sum
            .checked_add(i64::from(left) * i64::from(right))
            .ok_or("i8 dot overflow")?;
    }
    i32::try_from(sum).map_err(|_| "i8 dot outside i32".to_owned())
}

fn literal_hamming(left: &[u8], right: &[u8]) -> Result<u32, String> {
    if left.len() != right.len() {
        return Err("Hamming length mismatch".to_owned());
    }
    let mut total = 0_u32;
    for (&left, &right) in left.iter().zip(right) {
        let xor = left ^ right;
        for bit in 0..8 {
            total = total
                .checked_add(u32::from((xor >> bit) & 1))
                .ok_or("Hamming count overflow")?;
        }
    }
    Ok(total)
}

fn reference_f32_dot(left: &[F32], right: &[F32]) -> Result<(f64, f64), String> {
    if left.len() != right.len() {
        return Err("f32 length mismatch".to_owned());
    }
    let mut reference = 0.0_f64;
    let mut magnitude = 0.0_f64;
    for (&left, &right) in left.iter().zip(right) {
        let product = f64::from(left.to_float()) * f64::from(right.to_float());
        reference += product;
        magnitude += product.abs();
    }
    Ok((reference, magnitude))
}

fn bit4_integer_dot(
    query: &[i8],
    query_sum: i32,
    codes: &[u8],
    prepared: bool,
) -> Result<i32, String> {
    if codes.len() != query.len().div_ceil(2) {
        return Err("Bit4 code length mismatch".to_owned());
    }
    if !query.len().is_multiple_of(2) && codes.last().is_some_and(|byte| byte & 0x0f != 0) {
        return Err("Bit4 non-zero padding".to_owned());
    }
    let coordinate_query = if prepared {
        deinterleave_bit4_query(query)
    } else {
        query.to_vec()
    };
    let mut dot = 0_i64;
    for (coordinate, &query_code) in coordinate_query.iter().enumerate() {
        let byte = *codes
            .get(coordinate / 2)
            .ok_or("Bit4 code coordinate out of range")?;
        let nibble = if coordinate.is_multiple_of(2) {
            byte >> 4
        } else {
            byte & 0x0f
        };
        dot += i64::from(query_code) * i64::from(2 * i32::from(nibble) - 15);
    }
    if prepared {
        let unsigned_dot = (dot + 15_i64 * i64::from(query_sum)) / 2;
        i32::try_from(2 * unsigned_dot - 15_i64 * i64::from(query_sum))
            .map_err(|_| "Bit4 prepared dot outside i32".to_owned())
    } else {
        i32::try_from(dot).map_err(|_| "Bit4 dot outside i32".to_owned())
    }
}

fn deinterleave_bit4_query(prepared: &[i8]) -> Vec<i8> {
    let mut coordinate = vec![0_i8; prepared.len()];
    let mut base = 0_usize;
    while base < prepared.len() {
        let block_len = (prepared.len() - base).min(32);
        let even_count = block_len.div_ceil(2);
        for local in 0..block_len {
            let source = if local.is_multiple_of(2) {
                base + local / 2
            } else {
                base + even_count + local / 2
            };
            if let (Some(output), Some(&value)) =
                (coordinate.get_mut(base + local), prepared.get(source))
            {
                *output = value;
            }
        }
        base += block_len;
    }
    coordinate
}

fn decode_f16(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = u32::from((bits >> 10) & 0x1f);
    let fraction = u32::from(bits & 0x03ff);
    let output = match exponent {
        0 if fraction == 0 => sign,
        0 => {
            let mut mantissa = fraction;
            let mut exponent = -14_i32;
            while mantissa & 0x0400 == 0 {
                mantissa <<= 1;
                exponent -= 1;
            }
            mantissa &= 0x03ff;
            sign | ((exponent + 127) as u32) << 23 | mantissa << 13
        }
        0x1f => sign | 0x7f80_0000 | fraction << 13,
        _ => sign | (exponent + 112) << 23 | fraction << 13,
    };
    f32::from_bits(output)
}

pub fn expected_quantization(input: &QuantInput) -> I25Expected {
    match input.scheme {
        QuantScheme::Bit4 => expected_bit4(input),
        QuantScheme::Int8 => expected_int8(input),
    }
}

fn expected_bit4(input: &QuantInput) -> I25Expected {
    let output_len = usize::try_from(input.output_len).unwrap_or(usize::MAX);
    let mut output_after = vec![input.sentinel; output_len];
    let row = input
        .row
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let query = input
        .query
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let output_status = {
        let expected = row.len().div_ceil(2);
        if output_len == expected {
            Ok(())
        } else {
            Err(PrimitiveStatus::OutputLength {
                expected: expected as u64,
                actual: input.output_len,
            })
        }
    };
    let status = validate_quant_vector(&row).and(output_status);
    let Err(status) = status else {
        let (codes, factors, exact_reconstruction) = encode_bit4(&row);
        output_after.clone_from(&codes);
        let (query_codes, query_sum, scale_half) =
            match prepare_bit4_oracle(&query, input.query_seed) {
                Ok(prepared) => prepared,
                Err(query_status) => {
                    return quant_error_expected(input, output_after, query_status);
                }
            };
        if input.code_len != codes.len() as u64 {
            return quant_error_expected(
                input,
                output_after,
                PrimitiveStatus::CodeLength {
                    expected: codes.len() as u64,
                    actual: input.code_len,
                },
            );
        }
        let reconstruction = reconstruct_bit4(&codes, factors, row.len());
        let integer = match bit4_integer_dot(&query_codes, query_sum, &codes, true) {
            Ok(integer) => integer,
            Err(detail) => {
                return quant_error_expected(
                    input,
                    output_after,
                    PrimitiveStatus::SegmentGeometry { detail },
                );
            }
        };
        let correction = f64::from(factors[0]) * f64::from(factors[2]);
        let estimate = (correction * scale_half * f64::from(integer)) as f32;
        let reference = row
            .iter()
            .zip(&query)
            .map(|(&left, &right)| f64::from(left) * f64::from(right))
            .sum::<f64>();
        let l2_error = l2_distance(&row, &reconstruction);
        let exact_grid_error = l2_distance_f64(&row, &exact_reconstruction);
        let norm = l2_norm(&row);
        let allowance = 1.0e-5 * norm.max(1.0);
        let l2_bound =
            (exact_grid_error + allowance).min(std::f64::consts::SQRT_2 * norm + allowance);
        let estimate_error = (f64::from(estimate) - reference).abs();
        let query_norm = l2_norm(&query);
        let magnitude = row
            .iter()
            .zip(&query)
            .map(|(&left, &right)| (f64::from(left) * f64::from(right)).abs())
            .sum::<f64>();
        let estimate_bound = if row.is_empty() {
            0.0
        } else {
            5.75 * 2.0_f64.powi(-4) / (row.len() as f64).sqrt() * norm * query_norm
                + 1.0e-5 * magnitude.max(1.0)
        };
        let success = QuantSuccess {
            code_bytes: codes,
            factor_bits: factors.map(F32::from_float).to_vec(),
            query_code_bytes: query_codes.iter().map(|&value| value as u8).collect(),
            query_code_sum: query_sum,
            query_scale: F64::from_float(scale_half),
            reconstruction: reconstruction.into_iter().map(F32::from_float).collect(),
            estimate: F32::from_float(estimate),
        };
        return I25Expected {
            input: input.clone(),
            result: QuantResult {
                status: PrimitiveStatus::Ok,
                output_after,
                success: Some(success),
            },
            l2_error: F64::from_float(l2_error),
            l2_bound: F64::from_float(l2_bound),
            estimate_reference: F64::from_float(reference),
            estimate_error: F64::from_float(estimate_error),
            estimate_bound: F64::from_float(estimate_bound),
        };
    };
    quant_error_expected(input, output_after, status)
}

fn expected_int8(input: &QuantInput) -> I25Expected {
    let output_len = usize::try_from(input.output_len).unwrap_or(usize::MAX);
    let mut output_after = vec![input.sentinel; output_len];
    let row = input
        .row
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let query = input
        .query
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let output_status = {
        if output_len == row.len() {
            Ok(())
        } else {
            Err(PrimitiveStatus::OutputLength {
                expected: row.len() as u64,
                actual: input.output_len,
            })
        }
    };
    let status = validate_quant_vector(&row).and(output_status);
    let Err(status) = status else {
        let (codes, scale, offset) = encode_int8(&row);
        output_after = codes.iter().map(|&value| value as u8).collect();
        let (query_codes, query_scale, query_sum) = match prepare_int8_oracle(&query) {
            Ok(prepared) => prepared,
            Err(query_status) => return quant_error_expected(input, output_after, query_status),
        };
        if input.code_len != codes.len() as u64 {
            return quant_error_expected(
                input,
                output_after,
                PrimitiveStatus::CodeLength {
                    expected: codes.len() as u64,
                    actual: input.code_len,
                },
            );
        }
        let reconstruction = codes
            .iter()
            .map(|&code| (f64::from(scale) * f64::from(code) + f64::from(offset)) as f32)
            .collect::<Vec<_>>();
        let integer_dot = match checked_i8_dot(&query_codes, &codes) {
            Ok(integer) => integer,
            Err(detail) => {
                return quant_error_expected(
                    input,
                    output_after,
                    PrimitiveStatus::SegmentGeometry { detail },
                );
            }
        };
        let estimate = (query_scale
            * (f64::from(scale) * f64::from(integer_dot)
                + f64::from(offset) * f64::from(query_sum))) as f32;
        let reference = row
            .iter()
            .zip(&query)
            .map(|(&left, &right)| f64::from(left) * f64::from(right))
            .sum::<f64>();
        let l2_error = l2_distance(&row, &reconstruction);
        let maximum_coordinate_error = row
            .iter()
            .zip(&reconstruction)
            .map(|(&left, &right)| (f64::from(left) - f64::from(right)).abs())
            .fold(0.0_f64, f64::max);
        let l2_bound = maximum_coordinate_error * (row.len() as f64).sqrt();
        let estimate_error = (f64::from(estimate) - reference).abs();
        let magnitude = row
            .iter()
            .zip(&query)
            .map(|(&left, &right)| (f64::from(left) * f64::from(right)).abs())
            .sum::<f64>();
        let estimate_bound = maximum_coordinate_error * l2_norm(&query) * (row.len() as f64).sqrt()
            + 1.0e-5 * magnitude.max(1.0);
        let success = QuantSuccess {
            code_bytes: output_after.clone(),
            factor_bits: vec![F32::from_float(scale), F32::from_float(offset)],
            query_code_bytes: query_codes.iter().map(|&value| value as u8).collect(),
            query_code_sum: query_sum,
            query_scale: F64::from_float(query_scale),
            reconstruction: reconstruction.into_iter().map(F32::from_float).collect(),
            estimate: F32::from_float(estimate),
        };
        return I25Expected {
            input: input.clone(),
            result: QuantResult {
                status: PrimitiveStatus::Ok,
                output_after,
                success: Some(success),
            },
            l2_error: F64::from_float(l2_error),
            l2_bound: F64::from_float(l2_bound),
            estimate_reference: F64::from_float(reference),
            estimate_error: F64::from_float(estimate_error),
            estimate_bound: F64::from_float(estimate_bound),
        };
    };
    quant_error_expected(input, output_after, status)
}

fn quant_error_expected(
    input: &QuantInput,
    output_after: Vec<u8>,
    status: PrimitiveStatus,
) -> I25Expected {
    I25Expected {
        input: input.clone(),
        result: QuantResult {
            status,
            output_after,
            success: None,
        },
        l2_error: F64::from_float(0.0),
        l2_bound: F64::from_float(0.0),
        estimate_reference: F64::from_float(0.0),
        estimate_error: F64::from_float(0.0),
        estimate_bound: F64::from_float(0.0),
    }
}

fn validate_quant_vector(values: &[f32]) -> Result<(), PrimitiveStatus> {
    if values.is_empty() {
        return Err(PrimitiveStatus::EmptyVector);
    }
    if values.len() > MAX_I8_DIMENSION {
        return Err(PrimitiveStatus::DimensionTooLarge {
            actual: values.len() as u64,
            maximum: MAX_I8_DIMENSION as u64,
        });
    }
    if let Some((index, _)) = values
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(PrimitiveStatus::NonFinite {
            index: index as u64,
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Bit4Event {
    threshold: f64,
    coordinate: usize,
    level: u8,
    magnitude: f64,
}

fn encode_bit4(values: &[f32]) -> (Vec<u8>, [f32; 3], Vec<f64>) {
    let mut norm_squared = 0.0_f64;
    let mut absolute_sum = 0.0_f64;
    let mut row_scale = 0.0_f64;
    let mut events = Vec::with_capacity(values.len().saturating_mul(7));
    for (coordinate, &value) in values.iter().enumerate() {
        let magnitude = f64::from(value).abs();
        norm_squared += f64::from(value) * f64::from(value);
        absolute_sum += magnitude;
        row_scale = row_scale.max(magnitude);
        if magnitude > 0.0 {
            for level in 1..=7_u8 {
                events.push(Bit4Event {
                    threshold: f64::from(level) / magnitude,
                    coordinate,
                    level,
                    magnitude,
                });
            }
        }
    }
    events.sort_unstable_by(|left, right| {
        left.threshold
            .total_cmp(&right.threshold)
            .then_with(|| left.coordinate.cmp(&right.coordinate))
            .then_with(|| left.level.cmp(&right.level))
    });
    let mut numerator = 0.5 * absolute_sum;
    let mut grid_norm_squared = 0.25 * values.len() as f64;
    let mut best_numerator = numerator;
    let mut best_score = numerator * numerator / grid_norm_squared;
    let mut best_events = 0_usize;
    let mut cursor = 0_usize;
    while cursor < events.len() {
        let threshold = events[cursor].threshold;
        while cursor < events.len() && events[cursor].threshold == threshold {
            numerator += events[cursor].magnitude;
            grid_norm_squared += 2.0 * f64::from(events[cursor].level);
            cursor += 1;
        }
        let score = numerator * numerator / grid_norm_squared;
        if score > best_score {
            best_score = score;
            best_numerator = numerator;
            best_events = cursor;
        }
    }
    let mut levels = vec![0_u8; values.len()];
    for event in events.iter().take(best_events) {
        if let Some(level) = levels.get_mut(event.coordinate) {
            *level = event.level;
        }
    }
    let mut doubled_grid = Vec::with_capacity(values.len());
    let mut codes = vec![0_u8; values.len().div_ceil(2)];
    for (coordinate, (&value, &level)) in values.iter().zip(&levels).enumerate() {
        let unsigned = if value < 0.0 { 7 - level } else { 8 + level };
        let doubled = 2_i32 * i32::from(unsigned) - 15;
        doubled_grid.push(f64::from(doubled));
        if let Some(byte) = codes.get_mut(coordinate / 2) {
            if coordinate.is_multiple_of(2) {
                *byte |= unsigned << 4;
            } else {
                *byte |= unsigned;
            }
        }
    }
    let factors = if norm_squared == 0.0 {
        [0.0, 0.0, 0.0]
    } else {
        [
            row_scale as f32,
            (norm_squared.sqrt() / row_scale) as f32,
            (norm_squared / (row_scale * best_numerator)) as f32,
        ]
    };
    let grid_norm = doubled_grid
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    let norm = norm_squared.sqrt();
    let exact_reconstruction = if norm_squared == 0.0 {
        vec![0.0; values.len()]
    } else {
        doubled_grid
            .iter()
            .map(|value| norm * value / grid_norm)
            .collect()
    };
    (codes, factors, exact_reconstruction)
}

fn reconstruct_bit4(codes: &[u8], factors: [f32; 3], dimension: usize) -> Vec<f32> {
    if factors[0] == 0.0 {
        return vec![0.0; dimension];
    }
    let doubled = (0..dimension)
        .map(|coordinate| {
            let byte = codes.get(coordinate / 2).copied().unwrap_or(0);
            let unsigned = if coordinate.is_multiple_of(2) {
                byte >> 4
            } else {
                byte & 0x0f
            };
            2_i32 * i32::from(unsigned) - 15
        })
        .collect::<Vec<_>>();
    let doubled_norm = doubled
        .iter()
        .map(|&value| f64::from(value) * f64::from(value))
        .sum::<f64>()
        .sqrt();
    let norm = f64::from(factors[0]) * f64::from(factors[1]);
    doubled
        .iter()
        .map(|&value| {
            (norm * f64::from(value) / doubled_norm).clamp(f64::from(f32::MIN), f64::from(f32::MAX))
                as f32
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_open(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^= value >> 31;
        ((value >> 11) as f64 + 0.5) / 9_007_199_254_740_992.0
    }
}

fn prepare_bit4_oracle(values: &[f32], seed: u64) -> Result<(Vec<i8>, i32, f64), PrimitiveStatus> {
    validate_quant_vector(values)?;
    let maximum = values
        .iter()
        .map(|value| value.abs())
        .fold(0.0_f32, f32::max);
    if maximum == 0.0 {
        return Ok((vec![0; values.len()], 0, 0.0));
    }
    let scale = f64::from(maximum) / 127.0;
    let mut random = SplitMix64::new(seed);
    let coordinate = values
        .iter()
        .map(|&value| {
            let scaled = f64::from(value) / scale;
            let lower = scaled.floor();
            let rounded = if random.next_open() < scaled - lower {
                lower + 1.0
            } else {
                lower
            };
            rounded.clamp(-127.0, 127.0) as i8
        })
        .collect::<Vec<_>>();
    let sum = coordinate.iter().map(|&value| i32::from(value)).sum();
    let mut interleaved = Vec::with_capacity(coordinate.len());
    for block in coordinate.chunks(32) {
        interleaved.extend(block.iter().step_by(2).copied());
        interleaved.extend(block.iter().skip(1).step_by(2).copied());
    }
    Ok((interleaved, sum, scale * 0.5))
}

fn encode_int8(values: &[f32]) -> (Vec<i8>, f32, f32) {
    let (minimum, maximum) = values.iter().copied().fold(
        (f32::INFINITY, f32::NEG_INFINITY),
        |(minimum, maximum), value| (minimum.min(value), maximum.max(value)),
    );
    if minimum == maximum {
        return (vec![0; values.len()], 0.0, minimum);
    }
    let mut scale = ((f64::from(maximum) - f64::from(minimum)) / 254.0) as f32;
    if scale == 0.0 {
        scale = f32::from_bits(1);
    }
    let offset = ((f64::from(maximum) + f64::from(minimum)) * 0.5) as f32;
    let codes = values
        .iter()
        .map(|&value| {
            ((f64::from(value) - f64::from(offset)) / f64::from(scale))
                .round()
                .clamp(-127.0, 127.0) as i8
        })
        .collect();
    (codes, scale, offset)
}

fn prepare_int8_oracle(values: &[f32]) -> Result<(Vec<i8>, f64, i32), PrimitiveStatus> {
    validate_quant_vector(values)?;
    let maximum = values
        .iter()
        .map(|value| value.abs())
        .fold(0.0_f32, f32::max);
    if maximum == 0.0 {
        return Ok((vec![0; values.len()], 0.0, 0));
    }
    let scale = f64::from(maximum) / 127.0;
    let codes = values
        .iter()
        .map(|&value| (f64::from(value) / scale).round().clamp(-127.0, 127.0) as i8)
        .collect::<Vec<_>>();
    let sum = codes.iter().map(|&value| i32::from(value)).sum();
    Ok((codes, scale, sum))
}

fn l2_norm(values: &[f32]) -> f64 {
    values
        .iter()
        .map(|&value| f64::from(value) * f64::from(value))
        .sum::<f64>()
        .sqrt()
}

fn l2_distance(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(&left, &right)| {
            let delta = f64::from(left) - f64::from(right);
            delta * delta
        })
        .sum::<f64>()
        .sqrt()
}

fn l2_distance_f64(left: &[f32], right: &[f64]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(&left, &right)| {
            let delta = f64::from(left) - right;
            delta * delta
        })
        .sum::<f64>()
        .sqrt()
}

pub fn expected_rescore(input: &RescoreInput) -> I26Expected {
    let store_tier = input.store.as_ref().map(|store| store.tier);
    let store_exact_rescore = input
        .store
        .as_ref()
        .is_some_and(|store| store.exact_rescore);
    let invalid = |detail: String| I26Expected {
        case_id: input.case_id,
        metric: input.metric,
        status: PrimitiveStatus::SegmentGeometry { detail },
        hits: Vec::new(),
        score_tolerances: Vec::new(),
        candidates_rescored: 0,
        coarse_bytes: 0,
        rescore_bytes: 0,
        total_bytes: 0,
        store_hits: Vec::new(),
        store_tier,
        store_exact_rescore,
    };
    let Ok(dimension) = usize::try_from(input.dimension) else {
        return invalid("dimension does not fit usize".to_owned());
    };
    let Ok(k) = usize::try_from(input.k) else {
        return invalid("k does not fit usize".to_owned());
    };
    if dimension == 0 || input.query.len() != dimension || k == 0 {
        return invalid(format!(
            "invalid rescore controls: dimension={} query={} k={}",
            input.dimension,
            input.query.len(),
            input.k
        ));
    }
    if !input.rows_row_major.len().is_multiple_of(dimension) {
        return invalid(format!(
            "row scalars {} are not divisible by dimension {dimension}",
            input.rows_row_major.len()
        ));
    }
    let row_count = input.rows_row_major.len() / dimension;
    if let Some(store) = &input.store
        && store.documents_by_row.len() != row_count
    {
        return invalid(format!(
            "Store document rows {} differ from primitive rows {row_count}",
            store.documents_by_row.len()
        ));
    }
    let (selected, coarse_count) = match &input.candidates {
        CandidateMode::Dense { coarse, oversample } => {
            if coarse.len() != row_count || *oversample == 0 {
                return invalid(format!(
                    "dense pool mismatch: scores={} rows={row_count} oversample={oversample}",
                    coarse.len()
                ));
            }
            if let Some((row, _)) = coarse
                .iter()
                .enumerate()
                .find(|(_, value)| !value.to_float().is_finite())
            {
                return I26Expected {
                    case_id: input.case_id,
                    metric: input.metric,
                    status: PrimitiveStatus::NonFiniteScore { row: row as u64 },
                    hits: Vec::new(),
                    score_tolerances: Vec::new(),
                    candidates_rescored: 0,
                    coarse_bytes: 0,
                    rescore_bytes: 0,
                    total_bytes: 0,
                    store_hits: Vec::new(),
                    store_tier,
                    store_exact_rescore,
                };
            }
            let Some(requested) = k.checked_mul(usize::try_from(*oversample).unwrap_or(usize::MAX))
            else {
                return invalid("dense candidate multiplication overflow".to_owned());
            };
            let count = row_count.min(requested);
            let mut rows = (0..row_count).collect::<Vec<_>>();
            rows.sort_unstable_by(|&left, &right| {
                coarse[right]
                    .to_float()
                    .total_cmp(&coarse[left].to_float())
                    .then_with(|| left.cmp(&right))
            });
            rows.truncate(count);
            (rows, count)
        }
        CandidateMode::Retained { rows, coarse } => {
            if rows.len() != coarse.len() {
                return I26Expected {
                    case_id: input.case_id,
                    metric: input.metric,
                    status: PrimitiveStatus::CandidateRowCount {
                        expected: coarse.len() as u64,
                        actual: rows.len() as u64,
                    },
                    hits: Vec::new(),
                    score_tolerances: Vec::new(),
                    candidates_rescored: 0,
                    coarse_bytes: 0,
                    rescore_bytes: 0,
                    total_bytes: 0,
                    store_hits: Vec::new(),
                    store_tier,
                    store_exact_rescore,
                };
            }
            if let Some((position, _)) = coarse
                .iter()
                .enumerate()
                .find(|(_, score)| !score.to_float().is_finite())
            {
                return I26Expected {
                    case_id: input.case_id,
                    metric: input.metric,
                    status: PrimitiveStatus::NonFiniteScore {
                        row: position as u64,
                    },
                    hits: Vec::new(),
                    score_tolerances: Vec::new(),
                    candidates_rescored: 0,
                    coarse_bytes: 0,
                    rescore_bytes: 0,
                    total_bytes: 0,
                    store_hits: Vec::new(),
                    store_tier,
                    store_exact_rescore,
                };
            }
            let converted = rows.iter().map(|&row| row as usize).collect::<Vec<_>>();
            if let Some(&row) = converted.iter().find(|&&row| row >= row_count) {
                return I26Expected {
                    case_id: input.case_id,
                    metric: input.metric,
                    status: PrimitiveStatus::CandidateRowOutOfRange {
                        row: row as u64,
                        rows: row_count as u64,
                    },
                    hits: Vec::new(),
                    score_tolerances: Vec::new(),
                    candidates_rescored: 0,
                    coarse_bytes: 0,
                    rescore_bytes: 0,
                    total_bytes: 0,
                    store_hits: Vec::new(),
                    store_tier,
                    store_exact_rescore,
                };
            }
            (converted, rows.len())
        }
    };
    if coarse_count < k {
        return invalid(format!(
            "rescore pool has {coarse_count} candidates, fewer than k={k}"
        ));
    }
    let query = input
        .query
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let mut scored = Vec::with_capacity(selected.len());
    for &row_id in &selected {
        let start = row_id.saturating_mul(dimension);
        let Some(row) = input.rows_row_major.get(start..start + dimension) else {
            return invalid("selected row slice was out of range".to_owned());
        };
        let row = row.iter().map(|value| value.to_float()).collect::<Vec<_>>();
        let (score, tolerance) = match input.metric {
            RescoreMetric::InnerProduct => {
                let mut score = 0.0_f64;
                let mut magnitude = 0.0_f64;
                for (&left, &right) in query.iter().zip(&row) {
                    let product = f64::from(left) * f64::from(right);
                    score += product;
                    magnitude += product.abs();
                }
                (score, 1.0e-5 * magnitude.max(1.0))
            }
            RescoreMetric::SquaredL2 => {
                let distance = query
                    .iter()
                    .zip(&row)
                    .map(|(&left, &right)| {
                        let delta = f64::from(left) - f64::from(right);
                        delta * delta
                    })
                    .sum::<f64>();
                (-distance, 0.0)
            }
        };
        if !score.is_finite() {
            return I26Expected {
                case_id: input.case_id,
                metric: input.metric,
                status: PrimitiveStatus::NonFiniteScore { row: row_id as u64 },
                hits: Vec::new(),
                score_tolerances: Vec::new(),
                candidates_rescored: 0,
                coarse_bytes: 0,
                rescore_bytes: 0,
                total_bytes: 0,
                store_hits: Vec::new(),
                store_tier,
                store_exact_rescore,
            };
        }
        scored.push((row_id, score, tolerance));
    }
    scored.sort_unstable_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.truncate(k);
    let hits = scored
        .iter()
        .map(|&(row, score, _)| PrimitiveRescoreHit {
            row: row as u32,
            score: F64::from_float(score),
        })
        .collect::<Vec<_>>();
    let store_hits = input.store.as_ref().map_or_else(Vec::new, |store| {
        let mut store_hits = hits
            .iter()
            .map(|hit| {
                let row_index = hit.row as usize;
                let score = if store.exact_rescore {
                    hit.score.to_float() as f32
                } else {
                    let start = row_index * dimension;
                    let row = input.rows_row_major[start..start + dimension]
                        .iter()
                        .map(|value| value.to_float())
                        .collect::<Vec<_>>();
                    let (codes, factors, _) = encode_bit4(&row);
                    let (query_codes, query_sum, scale_half) =
                        prepare_bit4_oracle(&query, 0).expect("validated finite rescore query");
                    let integer = bit4_integer_dot(&query_codes, query_sum, &codes, true)
                        .expect("oracle-generated Bit4 geometry");
                    let correction = f64::from(factors[0]) * f64::from(factors[2]);
                    (correction * scale_half * f64::from(integer)) as f32
                };
                StoreRescoreHit {
                    row: PrimitiveRow {
                        source: store.source,
                        local_row: hit.row,
                    },
                    document: store.documents_by_row[row_index],
                    score: F32::from_float(score),
                    exact_score: store.exact_rescore,
                }
            })
            .collect::<Vec<_>>();
        store_hits.sort_unstable_by(|left, right| {
            right
                .score
                .to_float()
                .total_cmp(&left.score.to_float())
                .then_with(|| {
                    match (left.document, right.document) {
                        (Some(left_document), Some(right_document)) => left_document
                            .doc_id_be
                            .cmp(&right_document.doc_id_be)
                            .then_with(|| right_document.revision.cmp(&left_document.revision)),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    }
                    .then_with(|| left.row.cmp(&right.row))
                })
        });
        store_hits
    });
    let score_tolerances = scored
        .iter()
        .map(|&(_, _, tolerance)| F64::from_float(tolerance))
        .collect();
    let Some(coarse_bytes) = input
        .coarse_rows_touched
        .checked_mul(input.coarse_bytes_per_row)
    else {
        return invalid("coarse byte count overflow".to_owned());
    };
    let Some(rescore_bytes) = (coarse_count as u64)
        .checked_mul(input.dimension)
        .and_then(|values| values.checked_mul(4))
    else {
        return invalid("rescore byte count overflow".to_owned());
    };
    let Some(total_bytes) = coarse_bytes.checked_add(rescore_bytes) else {
        return invalid("total byte count overflow".to_owned());
    };
    I26Expected {
        case_id: input.case_id,
        metric: input.metric,
        status: PrimitiveStatus::Ok,
        hits,
        score_tolerances,
        candidates_rescored: coarse_count as u64,
        coarse_bytes,
        rescore_bytes,
        total_bytes,
        store_hits,
        store_tier,
        store_exact_rescore,
    }
}

pub fn expected_identity(input: &IdentityInput) -> Result<I27Expected, String> {
    #[derive(Clone, Copy)]
    struct State {
        document: PrimitiveDocument,
        row: PrimitiveRow,
        phase: u8,
    }

    let mut visible = BTreeMap::<[u8; 16], State>::new();
    let mut forbidden = BTreeSet::<PrimitiveDocument>::new();
    let mut active_row = 0_u32;
    let mut sealed_phase = 0_u8;
    for mutation in &input.mutations {
        match *mutation {
            IdentityMutation::Ingest(document) | IdentityMutation::Replace(document) => {
                if let Some(previous) = visible.get(&document.doc_id_be).copied() {
                    if document.revision < previous.document.revision {
                        return Err(format!(
                            "stale revision {} follows {}",
                            document.revision, previous.document.revision
                        ));
                    }
                    if document.revision == previous.document.revision {
                        continue;
                    }
                    forbidden.insert(previous.document);
                }
                let row = PrimitiveRow {
                    source: PrimitiveSource::Active,
                    local_row: active_row,
                };
                active_row = active_row.checked_add(1).ok_or("active row overflow")?;
                visible.insert(
                    document.doc_id_be,
                    State {
                        document,
                        row,
                        phase: sealed_phase,
                    },
                );
            }
            IdentityMutation::Delete {
                doc_id_be,
                revision,
            } => {
                let Some(previous) = visible.get(&doc_id_be).copied() else {
                    continue;
                };
                if previous.document.revision != revision {
                    return Err(format!(
                        "delete revision {revision} does not name visible revision {}",
                        previous.document.revision
                    ));
                }
                visible.remove(&doc_id_be);
                forbidden.insert(previous.document);
            }
            IdentityMutation::Seal(segment) => {
                sealed_phase = sealed_phase.checked_add(1).ok_or("phase overflow")?;
                for state in visible.values_mut() {
                    if state.row.source == PrimitiveSource::Active {
                        state.row.source = PrimitiveSource::Sealed(segment);
                        state.phase = sealed_phase;
                    }
                }
                active_row = 0;
            }
            IdentityMutation::Reopen => {}
        }
    }
    let visible = visible
        .into_values()
        .map(|state| IdentityExpectedRow {
            document: state.document,
            row: state.row,
            phase: state.phase,
        })
        .collect();
    let generation = public_generation(&input.public_schedule, 0)?;
    Ok(I27Expected {
        case_id: input.case_id,
        visible,
        forbidden: forbidden.into_iter().collect(),
        generation,
        phase: input.observation_phase,
        tier: input.tier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quant_store_input(
        generation_before: u64,
        schedule: Vec<PublicStoreStep>,
        document: Option<PrimitiveDocument>,
    ) -> QuantStoreInput {
        let document_visible = document.is_some();
        QuantStoreInput {
            generation_before,
            schedule,
            document,
            document_visible,
        }
    }

    #[test]
    fn canonical_attestation_and_structured_first_difference_cover_i24_i27() {
        assert_eq!(
            canonical_sha256(b"abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
        let kernel_input = KernelInput {
            case_id: 24,
            backend: BackendId::Scalar,
            selected_for_store: false,
            work_items: 2,
            kernel: KernelId::DotI8,
            dimension: 2,
            signed_a: vec![1, 2],
            signed_b: vec![3, 4],
            bytes_a: Vec::new(),
            bytes_b: Vec::new(),
            f32_a: Vec::new(),
            f32_b: Vec::new(),
            f16_a: Vec::new(),
            f16_b: Vec::new(),
            row_bytes: 0,
            batch_rows: 0,
            pointer_order: Vec::new(),
            query_sum: 0,
            query_scale_half: F64::from_float(0.0),
            bit4_factors: Vec::new(),
        };
        let kernel_expected = expected_kernel(&kernel_input).expect("literal kernel expected");
        let mut kernel_observed = I24Observed {
            case_id: 24,
            backend: BackendId::Scalar,
            kernel: KernelId::DotI8,
            value: KernelValue::S32(11),
            selected_for_store: false,
            work_items: 2,
        };

        let quant_input = QuantInput {
            case_id: 25,
            scheme: QuantScheme::Bit4,
            row: vec![F32::from_float(1.0)],
            query: vec![F32::from_float(1.0)],
            query_seed: 7,
            output_len: 1,
            code_len: 1,
            sentinel: 0xa5,
            store: quant_store_input(0, Vec::new(), None),
        };
        let quant_expected = expected_quantization(&quant_input);
        let mut quant_observed = I25Observed {
            case_id: 25,
            result: quant_expected.result.clone(),
            store: QuantStoreFacts {
                ingest_status: PrimitiveStatus::Ok,
                scan_status: PrimitiveStatus::Ok,
                generation_before: 0,
                generation_after: 0,
                document_visible: false,
                persisted_code_bytes: Vec::new(),
                persisted_factor_bits: Vec::new(),
            },
        };

        let rescore_input = RescoreInput {
            case_id: 26,
            metric: RescoreMetric::SquaredL2,
            query: vec![F32::from_float(1.0)],
            rows_row_major: vec![F32::from_float(1.0)],
            dimension: 1,
            k: 1,
            candidates: CandidateMode::Retained {
                rows: vec![0],
                coarse: vec![F32::from_float(0.0)],
            },
            coarse_rows_touched: 0,
            coarse_bytes_per_row: 0,
            store: None,
        };
        let rescore_expected = expected_rescore(&rescore_input);
        let mut rescore_observed = I26Observed {
            case_id: 26,
            primitive_status: rescore_expected.status.clone(),
            primitive_hits: rescore_expected.hits.clone(),
            primitive_counts: [
                rescore_expected.candidates_rescored,
                rescore_expected.coarse_bytes,
                rescore_expected.rescore_bytes,
                rescore_expected.total_bytes,
            ],
            tier: 0,
            store_hits: Vec::new(),
            exact_rescore: false,
        };

        let identity_input = IdentityInput {
            case_id: 27,
            mutations: Vec::new(),
            query: Vec::new(),
            k: 0,
            tier: 1,
            observation_phase: 0,
            public_schedule: Vec::new(),
        };
        let identity_expected = expected_identity(&identity_input).expect("identity expected");
        let mut identity_observed = I27Observed {
            case_id: 27,
            rows: Vec::new(),
            generation: 0,
            phase: 0,
            tier: 1,
            control_rows: Vec::new(),
            control_generation: 0,
            retry_rows: Vec::new(),
            retry_generation: 0,
            fault_status: PrimitiveStatus::Ok,
            retry_status: PrimitiveStatus::Ok,
        };

        let records = [
            canonical_i24_input(&kernel_input),
            canonical_i24_observed(&kernel_observed),
            canonical_i25_input(&quant_input),
            canonical_i25_observed(&quant_observed),
            canonical_i26_input(&rescore_input),
            canonical_i26_observed(&rescore_observed),
            canonical_i27_input(&identity_input),
            canonical_i27_observed(&identity_observed),
        ];
        assert!(records.iter().all(|record| {
            record.version == VECTOR_CANONICAL_VERSION
                && record
                    .bytes
                    .starts_with(VECTOR_CANONICAL_VERSION.as_bytes())
                && record.sha256.iter().any(|byte| *byte != 0)
        }));
        for pair in records.chunks_exact(2) {
            let replay = replay_canonical_comparison(&pair[0].bytes, &pair[1].bytes)
                .expect("decode and execute retained canonical comparison");
            assert!(replay.first_difference().is_none());
        }
        let mut corrupted = records[1].bytes.clone();
        corrupted.push(0);
        assert!(
            replay_canonical_comparison(&records[0].bytes, &corrupted).is_err(),
            "retained replay accepted trailing observed bytes"
        );

        kernel_observed.value = KernelValue::S32(12);
        quant_observed.case_id = 250;
        rescore_observed.primitive_counts[3] += 1;
        identity_observed.retry_generation += 1;
        for (difference, checker, path) in [
            (
                first_difference_i24(&kernel_expected, &kernel_observed),
                I24_CHECKER_ID,
                "value",
            ),
            (
                first_difference_i25(&quant_expected, &quant_observed),
                I25_CHECKER_ID,
                "case_id",
            ),
            (
                first_difference_i26(&rescore_expected, &rescore_observed),
                I26_CHECKER_ID,
                "primitive_counts.total_bytes",
            ),
            (
                first_difference_i27(&identity_expected, &identity_observed),
                I27_CHECKER_ID,
                "retry_generation",
            ),
        ] {
            let difference = difference.expect("deliberate canonical difference");
            assert_eq!(difference.checker_id, checker);
            assert_eq!(difference.path, path);
            assert_ne!(difference.expected, difference.observed);
        }
    }

    #[test]
    fn i24_checker_plant() {
        let expected = I24Expected {
            case_id: 24,
            backend: BackendId::Scalar,
            kernel: KernelId::DotI8,
            selected_for_store: false,
            work_items: 2,
            exact: KernelValue::S32(11),
            reference: None,
            magnitude: None,
            tolerance: None,
        };
        let planted = I24Observed {
            case_id: 24,
            backend: BackendId::Scalar,
            kernel: KernelId::DotI8,
            value: KernelValue::S32(12),
            selected_for_store: false,
            work_items: 2,
        };
        let error = check_i24(&expected, &planted).expect_err("the I24 plant must fire");
        assert_eq!(error.checker_id, I24_CHECKER_ID);
        let clean = I24Observed {
            value: KernelValue::S32(11),
            ..planted
        };
        check_i24(&expected, &clean).expect("unmutated I24 observation");
    }

    #[test]
    fn i24_backend_selection_and_work_are_checked() {
        let expected = I24Expected {
            case_id: 241,
            backend: BackendId::Scalar,
            kernel: KernelId::DotI8,
            selected_for_store: true,
            work_items: 2,
            exact: KernelValue::S32(11),
            reference: None,
            magnitude: None,
            tolerance: None,
        };
        let clean = I24Observed {
            case_id: 241,
            backend: BackendId::Scalar,
            kernel: KernelId::DotI8,
            value: KernelValue::S32(11),
            selected_for_store: true,
            work_items: 2,
        };
        for planted in [
            I24Observed {
                backend: BackendId::Avx2,
                ..clean.clone()
            },
            I24Observed {
                selected_for_store: false,
                ..clean.clone()
            },
            I24Observed {
                work_items: 1,
                ..clean.clone()
            },
        ] {
            let error = check_i24(&expected, &planted)
                .expect_err("backend, selection, and work plants must fire");
            assert_eq!(error.checker_id, I24_CHECKER_ID);
        }
        check_i24(&expected, &clean).expect("complete I24 observation");
    }

    #[test]
    fn i25_checker_plant() {
        let input = QuantInput {
            case_id: 25,
            scheme: QuantScheme::Bit4,
            row: vec![F32::from_float(f32::NAN)],
            query: vec![F32::from_float(1.0)],
            query_seed: 0,
            output_len: 1,
            code_len: 1,
            sentinel: 0xa5,
            store: quant_store_input(7, vec![PublicStoreStep::IngestRejected], None),
        };
        let expected = expected_quantization(&input);
        let store = QuantStoreFacts {
            ingest_status: PrimitiveStatus::NonFinite { index: 0 },
            scan_status: PrimitiveStatus::Ok,
            generation_before: 7,
            generation_after: 7,
            document_visible: false,
            persisted_code_bytes: Vec::new(),
            persisted_factor_bits: Vec::new(),
        };
        let planted = I25Observed {
            case_id: 25,
            result: QuantResult {
                status: PrimitiveStatus::Ok,
                output_after: vec![0xa5],
                success: None,
            },
            store: store.clone(),
        };
        let error = check_i25(&expected, &planted).expect_err("the I25 plant must fire");
        assert_eq!(error.checker_id, I25_CHECKER_ID);
        let clean = I25Observed {
            case_id: 25,
            result: expected.result.clone(),
            store,
        };
        check_i25(&expected, &clean).expect("unmutated I25 observation");
    }

    #[test]
    fn i25_rejected_document_remains_identified_but_invisible() {
        let input = QuantInput {
            case_id: 0x25_26,
            scheme: QuantScheme::Bit4,
            row: vec![F32::from_float(f32::NAN)],
            query: vec![F32::from_float(1.0)],
            query_seed: 0,
            output_len: 1,
            code_len: 1,
            sentinel: 0xa5,
            store: QuantStoreInput {
                generation_before: 0,
                schedule: vec![PublicStoreStep::IngestRejected],
                document: Some(PrimitiveDocument {
                    doc_id_be: [0x26; 16],
                    revision: 1,
                }),
                document_visible: false,
            },
        };
        let expected = expected_quantization(&input);
        check_i25(
            &expected,
            &I25Observed {
                case_id: input.case_id,
                result: expected.result.clone(),
                store: QuantStoreFacts {
                    ingest_status: PrimitiveStatus::NonFinite { index: 0 },
                    scan_status: PrimitiveStatus::Ok,
                    generation_before: 0,
                    generation_after: 0,
                    document_visible: false,
                    persisted_code_bytes: Vec::new(),
                    persisted_factor_bits: Vec::new(),
                },
            },
        )
        .expect("rejected attempted document must remain absent");
    }

    #[test]
    fn i25_nibble_checker_plant() {
        let input = QuantInput {
            case_id: 251,
            scheme: QuantScheme::Bit4,
            row: vec![
                F32::from_float(1.0),
                F32::from_float(-1.0),
                F32::from_float(0.0),
            ],
            query: vec![
                F32::from_float(1.0),
                F32::from_float(-1.0),
                F32::from_float(0.0),
            ],
            query_seed: 11,
            output_len: 2,
            code_len: 2,
            sentinel: 0xa5,
            store: quant_store_input(
                7,
                vec![PublicStoreStep::IngestAccepted],
                Some(PrimitiveDocument {
                    doc_id_be: [0x25; 16],
                    revision: 1,
                }),
            ),
        };
        let expected = expected_quantization(&input);
        let success = expected.result.success.as_ref().expect("Bit4 success");
        let store = QuantStoreFacts {
            ingest_status: PrimitiveStatus::Ok,
            scan_status: PrimitiveStatus::Ok,
            generation_before: 7,
            generation_after: 8,
            document_visible: true,
            persisted_code_bytes: success.code_bytes.clone(),
            persisted_factor_bits: success.factor_bits.clone(),
        };
        let mut planted_result = expected.result.clone();
        let planted_success = planted_result.success.as_mut().expect("Bit4 success");
        planted_success.code_bytes[0] ^= 0x10;
        let planted = I25Observed {
            case_id: input.case_id,
            result: planted_result,
            store: store.clone(),
        };
        let error = check_i25(&expected, &planted).expect_err("the I25 nibble plant must fire");
        assert_eq!(error.checker_id, I25_CHECKER_ID);
        check_i25(
            &expected,
            &I25Observed {
                case_id: input.case_id,
                result: expected.result.clone(),
                store,
            },
        )
        .expect("unmutated I25 observation");
    }

    #[test]
    fn i25_public_schedule_and_document_checker_plants() {
        let input = QuantInput {
            case_id: 254,
            scheme: QuantScheme::Int8,
            row: vec![F32::from_float(-1.0), F32::from_float(1.0)],
            query: vec![F32::from_float(1.0), F32::from_float(-1.0)],
            query_seed: 0,
            output_len: 2,
            code_len: 2,
            sentinel: 0xa5,
            store: quant_store_input(7, vec![PublicStoreStep::PublishPreparedSegment], None),
        };
        let expected = expected_quantization(&input);
        let success = expected.result.success.as_ref().expect("Int8 success");
        let clean = I25Observed {
            case_id: input.case_id,
            result: expected.result.clone(),
            store: QuantStoreFacts {
                ingest_status: PrimitiveStatus::Ok,
                scan_status: PrimitiveStatus::Ok,
                generation_before: 7,
                generation_after: 8,
                document_visible: false,
                persisted_code_bytes: success.code_bytes.clone(),
                persisted_factor_bits: success.factor_bits.clone(),
            },
        };
        for planted in [
            I25Observed {
                store: QuantStoreFacts {
                    generation_after: 9,
                    ..clean.store.clone()
                },
                ..clean.clone()
            },
            I25Observed {
                store: QuantStoreFacts {
                    document_visible: true,
                    ..clean.store.clone()
                },
                ..clean.clone()
            },
        ] {
            let error = check_i25(&expected, &planted)
                .expect_err("public schedule/document plants must fire");
            assert_eq!(error.checker_id, I25_CHECKER_ID);
        }
        check_i25(&expected, &clean).expect("unmutated public Int8 Store facts");
    }

    #[test]
    fn i25_code_length_is_exact_for_both_formats() {
        let bit4 = expected_quantization(&QuantInput {
            case_id: 252,
            scheme: QuantScheme::Bit4,
            row: vec![
                F32::from_float(1.0),
                F32::from_float(-1.0),
                F32::from_float(0.0),
            ],
            query: vec![
                F32::from_float(1.0),
                F32::from_float(-1.0),
                F32::from_float(0.0),
            ],
            query_seed: 11,
            output_len: 2,
            code_len: 1,
            sentinel: 0xa5,
            store: quant_store_input(0, vec![PublicStoreStep::IngestAccepted], None),
        });
        assert_eq!(
            bit4.result.status,
            PrimitiveStatus::CodeLength {
                expected: 2,
                actual: 1,
            }
        );

        let int8 = expected_quantization(&QuantInput {
            case_id: 253,
            scheme: QuantScheme::Int8,
            row: vec![
                F32::from_float(-1.0),
                F32::from_float(0.0),
                F32::from_float(1.0),
            ],
            query: vec![
                F32::from_float(-1.0),
                F32::from_float(0.0),
                F32::from_float(1.0),
            ],
            query_seed: 0,
            output_len: 3,
            code_len: 4,
            sentinel: 0xa5,
            store: quant_store_input(0, vec![PublicStoreStep::PublishPreparedSegment], None),
        });
        assert_eq!(
            int8.result.status,
            PrimitiveStatus::CodeLength {
                expected: 3,
                actual: 4,
            }
        );
    }

    #[test]
    fn i26_checker_plant() {
        let hits = vec![
            PrimitiveRescoreHit {
                row: 0,
                score: F64::from_float(1.0),
            },
            PrimitiveRescoreHit {
                row: 1,
                score: F64::from_float(1.0),
            },
        ];
        let first = StoreRescoreHit {
            row: PrimitiveRow {
                source: PrimitiveSource::Active,
                local_row: 0,
            },
            document: Some(PrimitiveDocument {
                doc_id_be: [1; 16],
                revision: 1,
            }),
            score: F32::from_float(1.0),
            exact_score: true,
        };
        let second = StoreRescoreHit {
            row: PrimitiveRow {
                source: PrimitiveSource::Active,
                local_row: 1,
            },
            document: Some(PrimitiveDocument {
                doc_id_be: [2; 16],
                revision: 1,
            }),
            score: F32::from_float(1.0),
            exact_score: true,
        };
        let expected = I26Expected {
            case_id: 26,
            metric: RescoreMetric::SquaredL2,
            status: PrimitiveStatus::Ok,
            hits: hits.clone(),
            score_tolerances: vec![F64::from_float(0.0); 2],
            candidates_rescored: 2,
            coarse_bytes: 2,
            rescore_bytes: 8,
            total_bytes: 10,
            store_hits: vec![first.clone(), second.clone()],
            store_tier: Some(1),
            store_exact_rescore: true,
        };
        let planted = I26Observed {
            case_id: 26,
            primitive_status: PrimitiveStatus::Ok,
            primitive_hits: hits,
            primitive_counts: [2, 2, 8, 10],
            tier: 1,
            store_hits: vec![second.clone(), first.clone()],
            exact_rescore: true,
        };
        let error = check_i26(&expected, &planted).expect_err("the I26 plant must fire");
        assert_eq!(error.checker_id, I26_CHECKER_ID);
        let clean = I26Observed {
            store_hits: vec![first, second],
            ..planted
        };
        check_i26(&expected, &clean).expect("unmutated I26 observation");
    }

    #[test]
    fn i26_dropped_candidate_checker_plant() {
        let input = RescoreInput {
            case_id: 262,
            metric: RescoreMetric::SquaredL2,
            query: vec![F32::from_float(0.0)],
            rows_row_major: vec![F32::from_float(0.0), F32::from_float(1.0)],
            dimension: 1,
            k: 2,
            candidates: CandidateMode::Retained {
                rows: vec![0, 1],
                coarse: vec![F32::from_float(2.0), F32::from_float(1.0)],
            },
            coarse_rows_touched: 2,
            coarse_bytes_per_row: 1,
            store: None,
        };
        let expected = expected_rescore(&input);
        let planted = I26Observed {
            case_id: input.case_id,
            primitive_status: expected.status.clone(),
            primitive_hits: expected.hits[..1].to_vec(),
            primitive_counts: [
                expected.candidates_rescored,
                expected.coarse_bytes,
                expected.rescore_bytes,
                expected.total_bytes,
            ],
            tier: 1,
            store_hits: expected.store_hits.clone(),
            exact_rescore: true,
        };
        let error =
            check_i26(&expected, &planted).expect_err("the dropped candidate plant must fire");
        assert_eq!(error.checker_id, I26_CHECKER_ID);
    }

    #[test]
    fn i26_public_tier_source_and_document_checker_plants() {
        let document = PrimitiveDocument {
            doc_id_be: [0x26; 16],
            revision: 3,
        };
        let source = PrimitiveSource::Sealed([0x62; 16]);
        let input = RescoreInput {
            case_id: 263,
            metric: RescoreMetric::SquaredL2,
            query: vec![F32::from_float(0.0)],
            rows_row_major: vec![F32::from_float(1.0)],
            dimension: 1,
            k: 1,
            candidates: CandidateMode::Retained {
                rows: vec![0],
                coarse: vec![F32::from_float(0.0)],
            },
            coarse_rows_touched: 1,
            coarse_bytes_per_row: 1,
            store: Some(RescoreStoreInput {
                source,
                documents_by_row: vec![Some(document)],
                tier: 3,
                exact_rescore: true,
            }),
        };
        let expected = expected_rescore(&input);
        let clean = I26Observed {
            case_id: input.case_id,
            primitive_status: expected.status.clone(),
            primitive_hits: expected.hits.clone(),
            primitive_counts: [
                expected.candidates_rescored,
                expected.coarse_bytes,
                expected.rescore_bytes,
                expected.total_bytes,
            ],
            tier: 3,
            store_hits: expected.store_hits.clone(),
            exact_rescore: true,
        };
        let mut wrong_source = clean.clone();
        wrong_source.store_hits[0].row.source = PrimitiveSource::Active;
        let mut wrong_document = clean.clone();
        wrong_document.store_hits[0].document = None;
        for planted in [
            I26Observed {
                tier: 1,
                ..clean.clone()
            },
            wrong_source,
            wrong_document,
        ] {
            let error = check_i26(&expected, &planted)
                .expect_err("public tier/source/document plants must fire");
            assert_eq!(error.checker_id, I26_CHECKER_ID);
        }
        check_i26(&expected, &clean).expect("unmutated public rescore facts");
    }

    #[test]
    fn i27_checker_plant() {
        let first_doc = PrimitiveDocument {
            doc_id_be: [1; 16],
            revision: 1,
        };
        let second_doc = PrimitiveDocument {
            doc_id_be: [2; 16],
            revision: 1,
        };
        let first_row = PrimitiveRow {
            source: PrimitiveSource::Sealed([9; 16]),
            local_row: 0,
        };
        let second_row = PrimitiveRow {
            source: PrimitiveSource::Sealed([9; 16]),
            local_row: 1,
        };
        let expected = I27Expected {
            case_id: 27,
            visible: vec![
                IdentityExpectedRow {
                    document: first_doc,
                    row: first_row,
                    phase: 1,
                },
                IdentityExpectedRow {
                    document: second_doc,
                    row: second_row,
                    phase: 1,
                },
            ],
            forbidden: Vec::new(),
            generation: 3,
            phase: 1,
            tier: 2,
        };
        let wrong_rows = vec![
            IdentityObservedRow {
                row: second_row,
                document: Some(first_doc),
                score: F32::from_float(1.0),
            },
            IdentityObservedRow {
                row: first_row,
                document: Some(second_doc),
                score: F32::from_float(1.0),
            },
        ];
        let planted = I27Observed {
            case_id: 27,
            rows: wrong_rows.clone(),
            generation: 3,
            phase: 1,
            tier: 2,
            control_rows: wrong_rows.clone(),
            control_generation: 3,
            retry_rows: wrong_rows,
            retry_generation: 3,
            fault_status: PrimitiveStatus::Ok,
            retry_status: PrimitiveStatus::Ok,
        };
        let error = check_i27(&expected, &planted).expect_err("the I27 plant must fire");
        assert_eq!(error.checker_id, I27_CHECKER_ID);
        let correct_rows = vec![
            IdentityObservedRow {
                row: first_row,
                document: Some(first_doc),
                score: F32::from_float(1.0),
            },
            IdentityObservedRow {
                row: second_row,
                document: Some(second_doc),
                score: F32::from_float(1.0),
            },
        ];
        let clean = I27Observed {
            rows: correct_rows.clone(),
            control_rows: correct_rows.clone(),
            retry_rows: correct_rows,
            ..planted
        };
        check_i27(&expected, &clean).expect("unmutated I27 observation");
    }

    #[test]
    fn i27_public_schedule_generation_checker_plant() {
        let document = PrimitiveDocument {
            doc_id_be: [0x27; 16],
            revision: 1,
        };
        let segment = [0x72; 16];
        let input = IdentityInput {
            case_id: 272,
            mutations: vec![
                IdentityMutation::Ingest(document),
                IdentityMutation::Seal(segment),
            ],
            query: vec![F32::from_float(0.0)],
            k: 1,
            tier: 1,
            observation_phase: 1,
            public_schedule: vec![PublicStoreStep::IngestAccepted, PublicStoreStep::Seal],
        };
        let expected = expected_identity(&input).expect("identity schedule");
        assert_eq!(expected.generation, 2);
        let rows = vec![IdentityObservedRow {
            row: PrimitiveRow {
                source: PrimitiveSource::Sealed(segment),
                local_row: 0,
            },
            document: Some(document),
            score: F32::from_float(0.0),
        }];
        let clean = I27Observed {
            case_id: input.case_id,
            rows: rows.clone(),
            generation: 2,
            phase: 1,
            tier: 1,
            control_rows: rows.clone(),
            control_generation: 2,
            retry_rows: rows,
            retry_generation: 2,
            fault_status: PrimitiveStatus::Ok,
            retry_status: PrimitiveStatus::Ok,
        };
        let planted = I27Observed {
            generation: 1,
            ..clean.clone()
        };
        let error =
            check_i27(&expected, &planted).expect_err("public schedule generation plant must fire");
        assert_eq!(error.checker_id, I27_CHECKER_ID);
        check_i27(&expected, &clean).expect("unmutated public schedule generation");
    }

    #[test]
    fn i27_checker_rejects_order_and_score_bit_mutations() {
        let segment = [0x27; 16];
        let first = PrimitiveDocument {
            doc_id_be: 10_u128.to_be_bytes(),
            revision: 1,
        };
        let second = PrimitiveDocument {
            doc_id_be: 20_u128.to_be_bytes(),
            revision: 1,
        };
        let input = IdentityInput {
            case_id: 27_900,
            mutations: vec![
                IdentityMutation::Ingest(first),
                IdentityMutation::Ingest(second),
                IdentityMutation::Seal(segment),
            ],
            query: vec![F32::from_float(1.0)],
            k: 2,
            tier: 1,
            observation_phase: 1,
            public_schedule: vec![
                PublicStoreStep::IngestAccepted,
                PublicStoreStep::IngestAccepted,
                PublicStoreStep::Seal,
            ],
        };
        let expected = expected_identity(&input).expect("identity expected facts");
        let rows = expected
            .visible
            .iter()
            .map(|row| IdentityObservedRow {
                row: row.row,
                document: Some(row.document),
                score: F32::from_float(0.0),
            })
            .collect::<Vec<_>>();
        let clean = I27Observed {
            case_id: input.case_id,
            rows: rows.clone(),
            generation: expected.generation,
            phase: expected.phase,
            tier: expected.tier,
            control_rows: rows.clone(),
            control_generation: expected.generation,
            retry_rows: rows.clone(),
            retry_generation: expected.generation,
            fault_status: PrimitiveStatus::Ok,
            retry_status: PrimitiveStatus::Ok,
        };
        check_i27(&expected, &clean).expect("ordered same-bit controls pass");

        let mut reordered = clean.clone();
        reordered.rows.swap(0, 1);
        let error = check_i27(&expected, &reordered).expect_err("order mutation must fail");
        assert_eq!(error.checker_id, I27_CHECKER_ID);

        let mut changed_score = clean;
        changed_score.retry_rows[0].score = F32(changed_score.retry_rows[0].score.0 ^ 1);
        let error = check_i27(&expected, &changed_score).expect_err("score-bit mutation must fail");
        assert_eq!(error.checker_id, I27_CHECKER_ID);
    }

    #[test]
    fn bit4_and_int8_expected_bytes_are_independent_goldens() {
        let bit4 = expected_quantization(&QuantInput {
            case_id: 250,
            scheme: QuantScheme::Bit4,
            row: vec![
                F32::from_float(1.0),
                F32::from_float(-1.0),
                F32::from_float(0.0),
            ],
            query: vec![
                F32::from_float(1.0),
                F32::from_float(-1.0),
                F32::from_float(0.0),
            ],
            query_seed: 11,
            output_len: 2,
            code_len: 2,
            sentinel: 0xa5,
            store: quant_store_input(0, vec![PublicStoreStep::IngestAccepted], None),
        });
        let bit4_success = bit4.result.success.expect("Bit4 success");
        assert_eq!(bit4_success.code_bytes, vec![0xf0, 0x80]);
        assert_eq!(bit4_success.code_bytes[1] & 0x0f, 0);

        let int8 = expected_quantization(&QuantInput {
            case_id: 251,
            scheme: QuantScheme::Int8,
            row: vec![
                F32::from_float(-1.0),
                F32::from_float(0.0),
                F32::from_float(1.0),
            ],
            query: vec![
                F32::from_float(-1.0),
                F32::from_float(0.0),
                F32::from_float(1.0),
            ],
            query_seed: 0,
            output_len: 3,
            code_len: 3,
            sentinel: 0xa5,
            store: quant_store_input(0, vec![PublicStoreStep::PublishPreparedSegment], None),
        });
        let int8_success = int8.result.success.expect("Int8 success");
        assert_eq!(int8_success.code_bytes, vec![129, 0, 127]);
        assert_eq!(int8_success.factor_bits[1], F32::from_float(0.0));
    }

    fn synthetic_persisted_segment(
        scheme: u16,
        dims: u32,
        row_stride: u32,
        factor_stride: u16,
        codes: &[u8],
        factors: &[u32],
    ) -> Vec<u8> {
        const FILE_HEADER_LEN: usize = 32;
        const SEGMENT_PREFIX_LEN: usize = 32;
        const REGION_ENTRY_LEN: usize = 32;
        const VECTOR_HEADER_LEN: usize = 32;
        let header_len = FILE_HEADER_LEN + SEGMENT_PREFIX_LEN + 2 * REGION_ENTRY_LEN + 8;
        let codes_offset = 160_usize;
        let factors_offset = 240_usize;
        let codes_len = VECTOR_HEADER_LEN + codes.len();
        let factors_len = VECTOR_HEADER_LEN + factors.len() * 4;
        let file_len = factors_offset + factors_len;
        let segment_id = [0x25_u8; 16];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ZEPEMBED");
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&(header_len as u64).to_le_bytes());
        bytes.extend_from_slice(&(file_len as u64).to_le_bytes());
        bytes.extend_from_slice(&segment_id);
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&scheme.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&dims.to_le_bytes());
        for (kind, offset, len) in [
            (3_u16, codes_offset, codes_len),
            (4_u16, factors_offset, factors_len),
        ] {
            bytes.extend_from_slice(&kind.to_le_bytes());
            bytes.extend_from_slice(&1_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes());
            bytes.extend_from_slice(&(offset as u64).to_le_bytes());
            bytes.extend_from_slice(&(len as u64).to_le_bytes());
            bytes.extend_from_slice(&0_u64.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.resize(codes_offset, 0);
        for payload in [codes, &[]] {
            bytes.extend_from_slice(&scheme.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&dims.to_le_bytes());
            bytes.extend_from_slice(&row_stride.to_le_bytes());
            bytes.extend_from_slice(&factor_stride.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u64.to_le_bytes());
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes());
            bytes.extend_from_slice(payload);
            if !payload.is_empty() {
                bytes.resize(factors_offset, 0);
            }
        }
        for factor in factors {
            bytes.extend_from_slice(&factor.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn persisted_quantization_is_parsed_from_literal_segment_bytes() {
        let bit4_bytes = synthetic_persisted_segment(
            4,
            3,
            2,
            12,
            &[0xf0, 0x80],
            &[1.0_f32.to_bits(), 2.0_f32.to_bits(), 0.5_f32.to_bits()],
        );
        let bit4 = parse_persisted_quantization(&bit4_bytes).expect("literal Bit4 segment");
        assert_eq!(bit4.segment_id, [0x25; 16]);
        assert_eq!(bit4.scheme, QuantScheme::Bit4);
        assert_eq!(bit4.dims, 3);
        assert_eq!(bit4.row_count, 1);
        assert_eq!(bit4.code_stride, 2);
        assert_eq!(bit4.factor_stride, 12);
        assert_eq!(bit4.code_bytes, vec![0xf0, 0x80]);
        assert_eq!(
            bit4.factor_bits,
            vec![
                F32::from_float(1.0),
                F32::from_float(2.0),
                F32::from_float(0.5),
            ]
        );

        let int8_bytes = synthetic_persisted_segment(
            2,
            3,
            3,
            8,
            &[129, 0, 127],
            &[0.25_f32.to_bits(), (-0.5_f32).to_bits()],
        );
        let int8 = parse_persisted_quantization(&int8_bytes).expect("literal Int8 segment");
        assert_eq!(int8.scheme, QuantScheme::Int8);
        assert_eq!(int8.code_bytes, vec![129, 0, 127]);
        assert_eq!(
            int8.factor_bits,
            vec![F32::from_float(0.25), F32::from_float(-0.5)]
        );
    }
}
