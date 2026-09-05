use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::bundle::Bundle;
use crate::epoch::{document_epoch, embedding_epoch};
use crate::error::TextError;
use crate::query::{Legs, QueryBackend, QueryOptions, TextHit, TextQueryOutcome, TextQueryTimings};
use crate::runtime::mlx::MlxRuntime;
use crate::runtime::{EmbeddingBatch, ModelRuntime, RuntimeError};
use crate::tower::{TokenBatch, TowerRole};
use zeppelin_embed::epoch::{EpochIdentity, Normalization, StoreEpoch};
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestAck, IngestBatch, IngestDocument, Revision,
    SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceReport, MaintenanceStatus};

const CALLER_BITS: u32 = 96;
/// Sequence length the bundled CoreML query tower is exported at.
const DEFAULT_COREML_TOKENS: usize = 64;
const DEFAULT_MAINTENANCE_BYTES: u64 = 256 * 1024 * 1024;

const CHUNK_BITS: u32 = 32;

/// One caller-owned text revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextDocument {
    /// Caller id occupying at most 96 bits.
    pub id: u128,
    /// Monotonic revision.
    pub revision: u64,
    /// UTF-8 text to store and embed.
    pub text: String,
}

impl TextDocument {
    /// Constructs one text revision.
    #[must_use]
    pub fn new(id: u128, revision: u64, text: impl Into<String>) -> Self {
        Self {
            id,
            revision,
            text: text.into(),
        }
    }
}

/// Long-document splitting policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkPolicy {
    /// Reject text whose model tokens exceed the tower maximum.
    None,
    /// Split into overlapping token-count windows.
    Tokens {
        /// Maximum approximate source tokens per chunk.
        max: usize,
        /// Repeated tokens between neighboring chunks.
        overlap: usize,
    },
}

/// Ingest pipeline controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IngestOptions {
    /// Documents tokenized per worker job and embedded per MLX call.
    pub embed_batch_size: usize,
    /// Core documents per ingest call and periodic seal boundary.
    pub seal_every: usize,
    /// Bounded tokenized-batch channel capacity.
    pub channel_capacity: usize,
    /// Long-document splitting policy.
    pub chunk_policy: ChunkPolicy,
    /// Maintenance wall-time allowance per seal.
    pub maintenance_wall_time: Duration,
    /// Maintenance byte allowance per seal.
    pub maintenance_bytes: u64,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            embed_batch_size: 32,
            seal_every: 4_096,
            channel_capacity: 2,
            chunk_policy: ChunkPolicy::Tokens {
                max: 510,
                overlap: 32,
            },
            maintenance_wall_time: Duration::from_millis(25),
            maintenance_bytes: DEFAULT_MAINTENANCE_BYTES,
        }
    }
}

/// Cooperative cancellation shared by every ingest stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum TextFaultSite {
    /// Panic inside the persistent embed worker command boundary.
    EmbedWorkerPanic,
    /// Close the tokenized-batch channel before all jobs are delivered.
    ChannelClosedEarly,
    /// Close the core store after ingest and before a periodic seal.
    SealFailureMidStream,
}

impl TextFaultSite {
    /// Stable adversarial coverage-registry key.
    #[doc(hidden)]
    #[must_use]
    pub const fn coverage_key(self) -> &'static str {
        match self {
            Self::EmbedWorkerPanic => "fault.site.embed-worker-panic",
            Self::ChannelClosedEarly => "fault.site.channel-closed-early",
            Self::SealFailureMidStream => "fault.site.seal-failure-mid-stream",
        }
    }

    const fn bit(self) -> usize {
        match self {
            Self::EmbedWorkerPanic => 1,
            Self::ChannelClosedEarly => 2,
            Self::SealFailureMidStream => 4,
        }
    }
}

/// Cooperative cancellation and deterministic fault control for one ingest call.
#[derive(Clone, Debug)]
pub struct IngestControl {
    cancelled: Arc<AtomicBool>,
    fault: Option<TextFaultSite>,
    fired: Arc<AtomicUsize>,
}

impl IngestControl {
    /// Constructs an active control.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            fault: None,
            fired: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Arms one deterministic adversarial pipeline site.
    #[doc(hidden)]
    #[must_use]
    pub fn with_fault(mut self, site: TextFaultSite) -> Self {
        self.fault = Some(site);
        self
    }

    /// Reports whether the armed site fired.
    #[doc(hidden)]
    #[must_use]
    pub fn fault_fired(&self, site: TextFaultSite) -> bool {
        self.fired.load(Ordering::Relaxed) & site.bit() != 0
    }

    /// Requests pipeline cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn fire(&self, site: TextFaultSite) -> bool {
        if self.fault != Some(site) {
            return false;
        }
        self.fired.fetch_or(site.bit(), Ordering::Relaxed) & site.bit() == 0
    }
}

impl Default for IngestControl {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic and measured facts from one completed ingest.
#[derive(Clone, Debug, PartialEq)]
pub struct TextIngestReport {
    /// Final acknowledged store generation.
    pub generation: u64,
    /// Caller documents consumed.
    pub documents: usize,
    /// Core chunk rows written.
    pub chunks: usize,
    /// Model tokens evaluated, including special tokens and padding.
    pub tokens: usize,
    /// MLX batch calls.
    pub embed_batches: usize,
    /// Periodic seal calls.
    pub seals: usize,
    /// Largest number of token batches concurrently buffered.
    pub max_in_flight_batches: usize,
    /// True only after every per-call worker was joined.
    pub all_threads_joined: bool,
    /// End-to-end wall time, for recorded baseline evidence only.
    pub elapsed: Duration,
}

/// Options that bind the lexical analyzer and core open policy together.
#[derive(Clone, Debug)]
pub struct TextOpenOptions {
    store: OpenOptions,
    lexical: TokenizerConfig,
}

impl TextOpenOptions {
    /// Creates text options around an existing core policy and analyzer.
    #[must_use]
    pub const fn new(store: OpenOptions, lexical: TokenizerConfig) -> Self {
        Self { store, lexical }
    }
}

impl Default for TextOpenOptions {
    fn default() -> Self {
        Self::new(OpenOptions::default(), TokenizerConfig::text_default())
    }
}

/// Store plus one bundle-bound tokenizer and persistent embed thread.
pub struct TextStore {
    store: Arc<Store>,
    bundle: Arc<Bundle>,
    analyzer: Analyzer,
    runtime: RuntimeClient,
    epoch: EpochIdentity,
    document_epoch: EpochIdentity,
    versions: Mutex<BTreeMap<DocId, Revision>>,
}

impl TextStore {
    /// Opens a `.zem` bundle and a store stamped with its two-tower epoch.
    pub fn open(
        store_path: impl AsRef<Path>,
        bundle_path: impl AsRef<Path>,
        options: TextOpenOptions,
    ) -> Result<Self, TextError> {
        let bundle_path = bundle_path.as_ref().to_path_buf();
        let bundle = Arc::new(Bundle::open(&bundle_path)?);
        let analyzer =
            Analyzer::new(options.lexical.clone()).map_err(|error| TextError::Pipeline {
                stage: "lexical analyzer",
                detail: error.to_string(),
            })?;
        let public_epoch = StoreEpoch {
            embedding: embedding_epoch(&bundle),
            tokenizer: analyzer.epoch(),
        };
        let epoch = public_epoch.identity();
        let store_epoch = StoreEpoch {
            embedding: document_epoch(&bundle),
            tokenizer: analyzer.epoch(),
        };
        let document_epoch = store_epoch.identity();
        let store = Arc::new(
            Store::open(
                store_path,
                options
                    .store
                    .with_epoch(store_epoch)
                    .with_tokenizer(options.lexical),
            )
            .map_err(TextError::Store)?,
        );
        let versions = load_versions(&store)?;
        let runtime =
            RuntimeClient::start(Arc::clone(&bundle), discover_query_coreml(&bundle_path))?;
        Ok(Self {
            store,
            bundle,
            analyzer,
            runtime,
            epoch,
            document_epoch,
            versions: Mutex::new(versions),
        })
    }

    /// Returns the immutable bundle-derived store identity.
    #[must_use]
    pub const fn epoch(&self) -> EpochIdentity {
        self.epoch
    }

    /// Returns the loaded query runtime and its requested compute policy.
    #[must_use]
    pub const fn query_backend(&self) -> QueryBackend {
        self.runtime.query_backend
    }

    /// Returns current store health without changing engine behavior.
    pub fn health(&self) -> Result<zeppelin_embed::diag::Health, TextError> {
        self.store.health().map_err(TextError::Store)
    }

    /// Runs due tier transitions within a host-supplied work budget.
    pub fn maintain(&self, budget: MaintenanceBudget) -> Result<MaintenanceReport, TextError> {
        let report = self.store.maintain(budget);
        if let MaintenanceStatus::Failed(error) = &report.status {
            return Err(TextError::Pipeline {
                stage: "maintenance",
                detail: error.to_string(),
            });
        }
        Ok(report)
    }

    /// Repeats bounded maintenance slices until every due transition completes.
    ///
    /// Each core call observes the supplied wall and byte limits. A slice that
    /// exhausts its budget without consuming any work fails loudly instead of
    /// spinning forever.
    pub fn maintain_to_completion(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, TextError> {
        if budget.wall_time.is_zero() {
            return Err(TextError::InvalidInput(
                "maintenance-to-completion wall time must be nonzero",
            ));
        }
        if budget.bytes == 0 {
            return Err(TextError::InvalidInput(
                "maintenance-to-completion byte budget must be nonzero",
            ));
        }
        let mut aggregate = empty_maintenance_report();
        loop {
            let mut slice = self.maintain(budget)?;
            let complete = matches!(slice.status, MaintenanceStatus::Complete);
            let progressed = slice.bytes_consumed != 0
                || slice.graphs_built != 0
                || slice.consolidations != 0
                || slice.passes_applied != 0;
            merge_maintenance_reports(&mut aggregate, &mut slice)?;
            if complete {
                aggregate.status = MaintenanceStatus::Complete;
                return Ok(aggregate);
            }
            if !progressed {
                return Err(TextError::Pipeline {
                    stage: "maintenance to completion",
                    detail: "a budget-exhausted slice made no progress".to_owned(),
                });
            }
        }
    }

    /// Runs bounded parallel tokenization, one MLX embed stream, caller-thread
    /// writes, periodic seals, and budgeted maintenance.
    pub fn ingest_text(
        &self,
        documents: &[TextDocument],
        options: IngestOptions,
    ) -> Result<TextIngestReport, TextError> {
        self.ingest_text_controlled(documents, options, IngestControl::new())
    }

    /// Serialized correctness control used to compare the pipelined path and
    /// to record the commit-1 end-to-end baseline.
    #[doc(hidden)]
    pub fn ingest_text_serialized(
        &self,
        documents: &[TextDocument],
        options: IngestOptions,
    ) -> Result<TextIngestReport, TextError> {
        validate_ingest_options(documents, options)?;
        let started = Instant::now();
        let jobs = build_jobs(
            documents,
            options,
            self.bundle.document_tower().embedding.max_tokens,
        )?;
        let mut core_batch = Vec::with_capacity(options.seal_every);
        let mut chunks = 0_usize;
        let mut tokens = 0_usize;
        let mut embed_batches = 0_usize;
        let mut seals = 0_usize;
        for job in jobs {
            let tokenized = tokenize_job(
                self.bundle.tokenizer(),
                job,
                self.bundle.document_tower().embedding.max_tokens as usize,
            )?;
            let TokenizedJob {
                ordinal: _,
                chunks: job_chunks,
                tokens: job_tokens,
                token_count,
            } = tokenized;
            let mut embedded = self.runtime.embed(TowerRole::Document, job_tokens, false)?;
            normalize_batch(&mut embedded, &self.bundle.document_tower().embedding)?;
            append_embedded(&job_chunks, embedded, &mut core_batch, &mut chunks)?;
            tokens = tokens.saturating_add(token_count);
            embed_batches = embed_batches.saturating_add(1);
            if core_batch.len() >= options.seal_every {
                self.store
                    .ingest(
                        IngestBatch::new(std::mem::take(&mut core_batch))
                            .with_epoch(self.document_epoch),
                    )
                    .map_err(TextError::Ingest)?;
                self.store.seal().map_err(TextError::Seal)?;
                seals = seals.saturating_add(1);
            }
        }
        if !core_batch.is_empty() {
            self.store
                .ingest(IngestBatch::new(core_batch).with_epoch(self.document_epoch))
                .map_err(TextError::Ingest)?;
            self.store.seal().map_err(TextError::Seal)?;
            seals = seals.saturating_add(1);
        }
        self.finish_ingest_maintenance(options)?;
        let generation = self.store.health().map_err(TextError::Store)?.generation;
        self.record_versions(documents, options.chunk_policy)?;
        Ok(TextIngestReport {
            generation,
            documents: documents.len(),
            chunks,
            tokens,
            embed_batches,
            seals,
            max_in_flight_batches: 0,
            all_threads_joined: true,
            elapsed: started.elapsed(),
        })
    }

    /// Runs ingest with an explicit cooperative cancellation token.
    pub fn ingest_text_controlled(
        &self,
        documents: &[TextDocument],
        options: IngestOptions,
        control: IngestControl,
    ) -> Result<TextIngestReport, TextError> {
        validate_ingest_options(documents, options)?;
        let started = Instant::now();
        let jobs = build_jobs(
            documents,
            options,
            self.bundle.document_tower().embedding.max_tokens,
        )?;
        let job_count = jobs.len();
        let worker_count = zeppelin_embed::scan::physical_thread_capacity()
            .map_err(|error| TextError::Pipeline {
                stage: "tokenizer capacity",
                detail: error.to_string(),
            })?
            .saturating_sub(2)
            .clamp(1, 4)
            .min(jobs.len().max(1));
        let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak_in_flight = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = mpsc::sync_channel(options.channel_capacity);
        let mut workers = Vec::with_capacity(worker_count);
        for worker in 0..worker_count {
            let queue = Arc::clone(&queue);
            let sender = sender.clone();
            let tokenizer = self.bundle.tokenizer().clone();
            let control = control.clone();
            let in_flight = Arc::clone(&in_flight);
            let peak = Arc::clone(&peak_in_flight);
            let max_tokens = self.bundle.document_tower().embedding.max_tokens as usize;
            let handle = std::thread::Builder::new()
                .name(format!("ze-text-tok-{worker}"))
                .spawn(move || {
                    loop {
                        if control.is_cancelled() {
                            break;
                        }
                        let job = match queue.lock() {
                            Ok(mut queue) => queue.pop_front(),
                            Err(_) => {
                                let _ = sender.send(Err(TextError::Pipeline {
                                    stage: "tokenizer queue",
                                    detail: "mutex poisoned".to_owned(),
                                }));
                                break;
                            }
                        };
                        let Some(job) = job else { break };
                        if control.fire(TextFaultSite::ChannelClosedEarly) {
                            break;
                        }
                        let result = tokenize_job(&tokenizer, job, max_tokens);
                        let Some(current) =
                            acquire_buffer_slot(&in_flight, options.channel_capacity, &control)
                        else {
                            break;
                        };
                        peak.fetch_max(current, Ordering::Relaxed);
                        if sender.send(result).is_err() {
                            in_flight.fetch_sub(1, Ordering::Relaxed);
                            break;
                        }
                    }
                })
                .map_err(|error| TextError::Pipeline {
                    stage: "tokenizer start",
                    detail: error.to_string(),
                })?;
            workers.push(handle);
        }
        drop(sender);
        let (maintenance_tx, maintenance_rx) = mpsc::sync_channel::<()>(1);
        let maintenance_store = Arc::clone(&self.store);
        let maintenance = std::thread::Builder::new()
            .name("ze-text-maintain".to_owned())
            .spawn(move || {
                while maintenance_rx.recv().is_ok() {
                    let _ = maintenance_store.maintain(MaintenanceBudget {
                        wall_time: options.maintenance_wall_time,
                        bytes: options.maintenance_bytes,
                    });
                }
            })
            .map_err(|error| TextError::Pipeline {
                stage: "maintenance start",
                detail: error.to_string(),
            })?;
        let mut pending = BTreeMap::new();
        let mut expected = 0_usize;
        let mut core_batch = Vec::with_capacity(options.seal_every);
        let mut chunks = 0_usize;
        let mut tokens = 0_usize;
        let mut embed_batches = 0_usize;
        let mut seals = 0_usize;
        let mut first_error = None;
        while let Ok(result) = receiver.recv() {
            in_flight.fetch_sub(1, Ordering::Relaxed);
            match result {
                Ok(job) => {
                    pending.insert(job.ordinal, job);
                    while let Some(job) = pending.remove(&expected) {
                        let TokenizedJob {
                            ordinal: _,
                            chunks: job_chunks,
                            tokens: job_tokens,
                            token_count,
                        } = job;
                        let inject_panic = control.fire(TextFaultSite::EmbedWorkerPanic);
                        match self
                            .runtime
                            .embed(TowerRole::Document, job_tokens, inject_panic)
                        {
                            Ok(mut embedded) => {
                                if let Err(error) = normalize_batch(
                                    &mut embedded,
                                    &self.bundle.document_tower().embedding,
                                ) {
                                    first_error = Some(error);
                                    control.cancel();
                                    break;
                                }
                                tokens = tokens.saturating_add(token_count);
                                embed_batches = embed_batches.saturating_add(1);
                                if let Err(error) = append_embedded(
                                    &job_chunks,
                                    embedded,
                                    &mut core_batch,
                                    &mut chunks,
                                ) {
                                    first_error = Some(error);
                                    control.cancel();
                                    break;
                                }
                                while core_batch.len() >= options.seal_every {
                                    let tail = core_batch.split_off(options.seal_every);
                                    let write = std::mem::replace(&mut core_batch, tail);
                                    match self.store.ingest(
                                        IngestBatch::new(write).with_epoch(self.document_epoch),
                                    ) {
                                        Ok(_) => {}
                                        Err(error) => {
                                            first_error = Some(TextError::Ingest(error));
                                            control.cancel();
                                            break;
                                        }
                                    }
                                    if control.fire(TextFaultSite::SealFailureMidStream) {
                                        if let Err(error) = self.store.close() {
                                            first_error = Some(TextError::Store(error));
                                            control.cancel();
                                            break;
                                        }
                                    }
                                    match self.store.seal() {
                                        Ok(_) => {}
                                        Err(error) => {
                                            first_error = Some(TextError::Seal(error));
                                            control.cancel();
                                            break;
                                        }
                                    }
                                    seals = seals.saturating_add(1);
                                    let _ = maintenance_tx.try_send(());
                                }
                            }
                            Err(error) => {
                                first_error = Some(error);
                                control.cancel();
                            }
                        }
                        expected = expected.saturating_add(1);
                        if first_error.is_some() {
                            break;
                        }
                    }
                }
                Err(error) => {
                    first_error = Some(error);
                    control.cancel();
                }
            }
        }
        let mut all_threads_joined = true;
        for worker in workers {
            if worker.join().is_err() {
                all_threads_joined = false;
                if first_error.is_none() {
                    first_error = Some(TextError::Pipeline {
                        stage: "tokenizer worker",
                        detail: "thread panicked".to_owned(),
                    });
                }
            }
        }
        if first_error.is_none() && expected != job_count {
            first_error = Some(TextError::Pipeline {
                stage: "tokenizer channel",
                detail: format!("channel closed after {expected} of {job_count} token batches"),
            });
        }
        if first_error.is_none() && !core_batch.is_empty() {
            self.store
                .ingest(IngestBatch::new(core_batch).with_epoch(self.document_epoch))
                .map_err(TextError::Ingest)?;
            if control.fire(TextFaultSite::SealFailureMidStream) {
                self.store.close().map_err(TextError::Store)?;
            }
            self.store.seal().map_err(TextError::Seal)?;
            seals = seals.saturating_add(1);
            let _ = maintenance_tx.try_send(());
        }
        drop(maintenance_tx);
        if maintenance.join().is_err() {
            all_threads_joined = false;
            if first_error.is_none() {
                first_error = Some(TextError::Pipeline {
                    stage: "maintenance worker",
                    detail: "thread panicked".to_owned(),
                });
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.finish_ingest_maintenance(options)?;
        let generation = self.store.health().map_err(TextError::Store)?.generation;
        self.record_versions(documents, options.chunk_policy)?;
        Ok(TextIngestReport {
            generation,
            documents: documents.len(),
            chunks,
            tokens,
            embed_batches,
            seals,
            max_in_flight_batches: peak_in_flight.load(Ordering::Relaxed),
            all_threads_joined,
            elapsed: started.elapsed(),
        })
    }

    fn finish_ingest_maintenance(&self, options: IngestOptions) -> Result<(), TextError> {
        if options.maintenance_wall_time.is_zero() || options.maintenance_bytes == 0 {
            return Ok(());
        }
        self.maintain_to_completion(MaintenanceBudget {
            wall_time: options.maintenance_wall_time,
            bytes: options.maintenance_bytes,
        })?;
        Ok(())
    }

    fn record_versions(
        &self,
        documents: &[TextDocument],
        chunk_policy: ChunkPolicy,
    ) -> Result<(), TextError> {
        let mut versions = self.versions.lock().map_err(|_| TextError::Pipeline {
            stage: "version registry",
            detail: "mutex poisoned".to_owned(),
        })?;
        for document in documents {
            let chunk_count = chunks_for_text(document, chunk_policy)?.len();
            for chunk in 0..chunk_count {
                let id = encode_chunk_id(
                    document.id,
                    u32::try_from(chunk)
                        .map_err(|_| TextError::InvalidInput("chunk count exceeds u32"))?,
                )?;
                versions.insert(id, Revision::new(document.revision));
            }
        }
        Ok(())
    }

    /// Tombstones every stored chunk belonging to each caller document id.
    pub fn delete_text(&self, caller_ids: &[u128]) -> Result<IngestAck, TextError> {
        if caller_ids.is_empty() {
            return Err(TextError::InvalidInput("delete caller id batch is empty"));
        }
        let mut versions = self.versions.lock().map_err(|_| TextError::Pipeline {
            stage: "version registry",
            detail: "mutex poisoned".to_owned(),
        })?;
        let mut chunk_ids = Vec::new();
        for caller_id in caller_ids {
            if caller_id >> CALLER_BITS != 0 {
                return Err(TextError::InvalidInput(
                    "caller document id exceeds 96 bits",
                ));
            }
            let encoded_caller = caller_id << CHUNK_BITS;
            let before = chunk_ids.len();
            chunk_ids.extend(
                versions
                    .keys()
                    .copied()
                    .filter(|id| id.get() >> CHUNK_BITS == *caller_id),
            );
            if chunk_ids.len() == before {
                chunk_ids.push(DocId::new(encoded_caller));
            }
        }
        chunk_ids.sort_unstable_by_key(|id| id.get());
        chunk_ids.dedup();
        let ack = self
            .store
            .delete(DeleteBatch::new(chunk_ids.clone()))
            .map_err(TextError::Ingest)?;
        for id in chunk_ids {
            versions.remove(&id);
        }
        Ok(ack)
    }

    /// Embeds and executes one dense, lexical, or hybrid text query.
    pub fn query_text(&self, text: &str, options: QueryOptions) -> Result<Vec<TextHit>, TextError> {
        self.query_text_with_diagnostics(text, options)
            .map(|outcome| outcome.hits)
    }

    /// Executes the same text query with core work and optional outer timing.
    /// Stage timing is enabled by the core `query-timing` feature. This does
    /// not change the query's tier, score policy, snapshot or embedding epoch.
    pub fn query_text_with_diagnostics(
        &self,
        text: &str,
        options: QueryOptions,
    ) -> Result<TextQueryOutcome, TextError> {
        let started = stage_start();
        let mut timings = TextQueryTimings::default();
        if options.k == 0 {
            timings.end_to_end = stage_elapsed(started);
            return Ok(TextQueryOutcome {
                hits: Vec::new(),
                diagnostics: None,
                backend: None,
                timings: zeppelin_embed::diag::QUERY_TIMING_ENABLED.then_some(timings),
                query_tokens: 0,
                embedding_calls: 0,
            });
        }
        let mut query_tokens = 0;
        let (hits, diagnostics) = match options.legs {
            Legs::Lexical => {
                let query = self.analyzed_query(text, &mut timings);
                self.lexical_hits(&query, options.k, &mut timings)?
            }
            Legs::Dense => {
                let (embedded, tokens) = self.query_vector(text, &mut timings)?;
                query_tokens = tokens;
                self.dense_hits(embedded.values(), options.k, options.tier, &mut timings)?
            }
            Legs::Hybrid => {
                let (embedded, tokens) = self.query_vector(text, &mut timings)?;
                query_tokens = tokens;
                let query = self.analyzed_query(text, &mut timings);
                self.hybrid_hits(
                    embedded.values(),
                    &query,
                    options.k,
                    options.tier,
                    &mut timings,
                )?
            }
        };
        timings.end_to_end = stage_elapsed(started);
        Ok(TextQueryOutcome {
            hits,
            diagnostics: Some(diagnostics),
            timings: zeppelin_embed::diag::QUERY_TIMING_ENABLED.then_some(timings),
            backend: (options.legs != Legs::Lexical).then_some(self.runtime.query_backend),
            query_tokens,
            embedding_calls: usize::from(options.legs != Legs::Lexical),
        })
    }

    fn analyzed_query(&self, text: &str, timings: &mut TextQueryTimings) -> TermQuery {
        let started = stage_start();
        let terms = self
            .analyzer
            .analyze(text)
            .into_iter()
            .map(|token| token.term.into_bytes())
            .collect();
        let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);
        timings.lexical_analysis = stage_elapsed(started);
        query
    }

    fn query_vector(
        &self,
        text: &str,
        timings: &mut TextQueryTimings,
    ) -> Result<(EmbeddingBatch, usize), TextError> {
        let started = stage_start();
        let tokens = self.bundle.tokenize_query(text)?;
        timings.tokenization = stage_elapsed(started);
        let count = tokens
            .attention_mask
            .iter()
            .filter(|mask| **mask != 0.0)
            .count();
        let evaluated = self
            .runtime
            .embed_with_timing(TowerRole::Query, tokens, false)?;
        timings.embedding_queue = evaluated.queue_wait;
        timings.embedding_evaluation = evaluated.evaluation;
        let mut embedded = evaluated.batch;
        let started = stage_start();
        normalize_batch(&mut embedded, &self.bundle.query_tower().embedding)?;
        timings.embedding_normalization = stage_elapsed(started);
        Ok((embedded, count))
    }

    /// Closes runtime resources and the underlying store.
    pub fn close(&self) -> Result<(), TextError> {
        self.runtime.close()?;
        self.store.close().map_err(TextError::Store)
    }

    fn dense_hits(
        &self,
        vector: &[f32],
        k: usize,
        tier: Option<zeppelin_embed::lifecycle::SearchTier>,
        timings: &mut TextQueryTimings,
    ) -> Result<(Vec<TextHit>, zeppelin_embed::diag::QueryDiagnostics), TextError> {
        let retrieval_started = stage_start();
        let outcome = self
            .store
            .search(
                SearchRequest::new(vector),
                k,
                Self::search_options(tier),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(TextError::Query)?;
        timings.retrieval = stage_elapsed(retrieval_started);
        let materialization_started = stage_start();
        let hits = outcome
            .candidates
            .into_iter()
            .map(|candidate| {
                let version = candidate.document().ok_or(TextError::Pipeline {
                    stage: "dense result",
                    detail: "candidate has no document identity".to_owned(),
                })?;
                let squared_l2 = -f64::from(candidate.score());
                self.make_hit(version, -squared_l2, Some(squared_l2), None)
            })
            .collect::<Result<Vec<_>, TextError>>()?;
        timings.materialization = stage_elapsed(materialization_started);
        Ok((hits, outcome.diagnostics))
    }

    fn lexical_hits(
        &self,
        query: &TermQuery,
        k: usize,
        timings: &mut TextQueryTimings,
    ) -> Result<(Vec<TextHit>, zeppelin_embed::diag::QueryDiagnostics), TextError> {
        let retrieval_started = stage_start();
        let outcome = self
            .store
            .search_lexical(query, k, QueryControl::Cancel(CancelToken::new()))
            .map_err(TextError::Lexical)?;
        timings.retrieval = stage_elapsed(retrieval_started);
        let materialization_started = stage_start();
        let hits = outcome
            .candidates
            .into_iter()
            .map(|candidate| {
                self.make_hit(
                    candidate.document,
                    candidate.score,
                    None,
                    Some(candidate.score),
                )
            })
            .collect::<Result<Vec<_>, TextError>>()?;
        timings.materialization = stage_elapsed(materialization_started);
        Ok((hits, outcome.diagnostics))
    }

    fn hybrid_hits(
        &self,
        vector: &[f32],
        query: &TermQuery,
        k: usize,
        tier: Option<zeppelin_embed::lifecycle::SearchTier>,
        timings: &mut TextQueryTimings,
    ) -> Result<(Vec<TextHit>, zeppelin_embed::diag::QueryDiagnostics), TextError> {
        let retrieval_started = stage_start();
        let outcome = self
            .store
            .search_hybrid(
                SearchRequest::new(vector),
                query,
                &HybridQuery::new(k)
                    .with_alpha(self.bundle.hybrid_alpha())
                    .with_epoch(self.document_epoch),
                Self::search_options(tier),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(TextError::Hybrid)?;
        timings.retrieval = stage_elapsed(retrieval_started);
        let materialization_started = stage_start();
        let versions = self.versions.lock().map_err(|_| TextError::Pipeline {
            stage: "version registry",
            detail: "mutex poisoned".to_owned(),
        })?;
        let hits = outcome
            .hits
            .into_iter()
            .map(|hit| {
                let revision = versions.get(&hit.key).copied().ok_or(TextError::Pipeline {
                    stage: "hybrid result",
                    detail: "document revision is unavailable".to_owned(),
                })?;
                self.make_hit(
                    DocumentVersion::new(hit.key, revision),
                    hit.fused_score,
                    hit.vector_squared_l2,
                    hit.lexical_bm25,
                )
            })
            .collect::<Result<Vec<_>, TextError>>()?;
        timings.materialization = stage_elapsed(materialization_started);
        Ok((hits, outcome.diagnostics))
    }

    /// Applies an explicit tier only when the caller asked for one.
    ///
    /// An unset tier must stay unset: each leg has its own contract for the
    /// no-preference case, and hybrid resolves it from graph availability.
    fn search_options(tier: Option<zeppelin_embed::lifecycle::SearchTier>) -> SearchOptions {
        match tier {
            Some(tier) => SearchOptions::default().with_tier(tier),
            None => SearchOptions::default(),
        }
    }

    fn make_hit(
        &self,
        version: DocumentVersion,
        score: f64,
        vector_squared_l2: Option<f64>,
        lexical_bm25: Option<f64>,
    ) -> Result<TextHit, TextError> {
        let encoded = version.doc_id().get();
        let doc_id = encoded >> CHUNK_BITS;
        let chunk = u32::try_from(encoded & u128::from(u32::MAX))
            .map_err(|_| TextError::InvalidInput("chunk id exceeds u32"))?;
        let text = self
            .store
            .stored_text(version)
            .map_err(TextError::Lexical)?
            .ok_or(TextError::Pipeline {
                stage: "stored text",
                detail: "result row has no stored text".to_owned(),
            })?;
        Ok(TextHit {
            doc_id,
            revision: version.revision().get(),
            chunk,
            text,
            score,
            vector_squared_l2,
            lexical_bm25,
            epoch: self.epoch,
        })
    }
}

fn empty_maintenance_report() -> MaintenanceReport {
    MaintenanceReport {
        graphs_built: 0,
        bytes_consumed: 0,
        checkpoints_resumed: 0,
        promotion_deferrals: Vec::new(),
        graph_profiles: Vec::new(),
        consolidations: 0,
        consolidation_generation: None,
        consolidation_deferrals: Vec::new(),
        passes_applied: 0,
        pass_counters: Default::default(),
        refinement_generation: None,
        status: MaintenanceStatus::Complete,
    }
}

fn merge_maintenance_reports(
    aggregate: &mut MaintenanceReport,
    slice: &mut MaintenanceReport,
) -> Result<(), TextError> {
    aggregate.graphs_built = checked_maintenance_sum(aggregate.graphs_built, slice.graphs_built)?;
    aggregate.bytes_consumed =
        checked_maintenance_sum(aggregate.bytes_consumed, slice.bytes_consumed)?;
    aggregate.checkpoints_resumed =
        checked_maintenance_sum(aggregate.checkpoints_resumed, slice.checkpoints_resumed)?;
    aggregate.consolidations =
        checked_maintenance_sum(aggregate.consolidations, slice.consolidations)?;
    aggregate.passes_applied =
        checked_maintenance_sum(aggregate.passes_applied, slice.passes_applied)?;
    aggregate.pass_counters.renumber = checked_maintenance_sum(
        aggregate.pass_counters.renumber,
        slice.pass_counters.renumber,
    )?;
    aggregate.pass_counters.alpha_reprune = checked_maintenance_sum(
        aggregate.pass_counters.alpha_reprune,
        slice.pass_counters.alpha_reprune,
    )?;
    aggregate.pass_counters.seed_refit = checked_maintenance_sum(
        aggregate.pass_counters.seed_refit,
        slice.pass_counters.seed_refit,
    )?;
    aggregate.pass_counters.neighbor_reorder = checked_maintenance_sum(
        aggregate.pass_counters.neighbor_reorder,
        slice.pass_counters.neighbor_reorder,
    )?;
    aggregate.pass_counters.connectivity_repair = checked_maintenance_sum(
        aggregate.pass_counters.connectivity_repair,
        slice.pass_counters.connectivity_repair,
    )?;
    aggregate
        .promotion_deferrals
        .append(&mut slice.promotion_deferrals);
    aggregate.graph_profiles.append(&mut slice.graph_profiles);
    aggregate
        .consolidation_deferrals
        .append(&mut slice.consolidation_deferrals);
    if slice.consolidation_generation.is_some() {
        aggregate.consolidation_generation = slice.consolidation_generation;
    }
    if slice.refinement_generation.is_some() {
        aggregate.refinement_generation = slice.refinement_generation;
    }
    Ok(())
}

fn checked_maintenance_sum(left: u64, right: u64) -> Result<u64, TextError> {
    left.checked_add(right).ok_or_else(|| TextError::Pipeline {
        stage: "maintenance to completion",
        detail: "maintenance report counter overflowed u64".to_owned(),
    })
}

fn validate_ingest_options(
    documents: &[TextDocument],
    options: IngestOptions,
) -> Result<(), TextError> {
    if documents.is_empty() {
        return Err(TextError::InvalidInput("document batch is empty"));
    }
    if options.embed_batch_size == 0 || options.seal_every == 0 || options.channel_capacity == 0 {
        return Err(TextError::InvalidInput(
            "pipeline capacities must be non-zero",
        ));
    }
    if let ChunkPolicy::Tokens { max, overlap } = options.chunk_policy
        && (max == 0 || overlap >= max)
    {
        return Err(TextError::InvalidInput(
            "chunk overlap must be smaller than max",
        ));
    }
    for document in documents {
        if document.id >> CALLER_BITS != 0 {
            return Err(TextError::InvalidInput(
                "caller document id exceeds 96 bits",
            ));
        }
    }
    Ok(())
}

fn acquire_buffer_slot(
    in_flight: &AtomicUsize,
    capacity: usize,
    control: &IngestControl,
) -> Option<usize> {
    loop {
        if control.is_cancelled() {
            return None;
        }
        let current = in_flight.load(Ordering::Relaxed);
        if current < capacity
            && in_flight
                .compare_exchange_weak(
                    current,
                    current.saturating_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            return Some(current.saturating_add(1));
        }
        std::thread::yield_now();
    }
}

#[derive(Clone)]
struct ChunkRow {
    id: u128,
    revision: u64,
    chunk: u32,
    text: String,
}

struct TokenJob {
    ordinal: usize,
    chunks: Vec<ChunkRow>,
}

struct TokenizedJob {
    ordinal: usize,
    chunks: Vec<ChunkRow>,
    tokens: TokenBatch,
    token_count: usize,
}

fn build_jobs(
    documents: &[TextDocument],
    options: IngestOptions,
    tower_max: u32,
) -> Result<Vec<TokenJob>, TextError> {
    let mut rows = Vec::new();
    for document in documents {
        for (chunk, text) in chunks_for_text(document, options.chunk_policy)?
            .into_iter()
            .enumerate()
        {
            let chunk = u32::try_from(chunk)
                .map_err(|_| TextError::InvalidInput("chunk count exceeds u32"))?;
            rows.push(ChunkRow {
                id: document.id,
                revision: document.revision,
                chunk,
                text,
            });
        }
    }
    let max = tower_max as usize;
    if matches!(options.chunk_policy, ChunkPolicy::None)
        && rows
            .iter()
            .any(|row| row.text.split_whitespace().count().saturating_add(2) > max)
    {
        return Err(TextError::InvalidInput("document exceeds tower max_tokens"));
    }
    Ok(rows
        .chunks(options.embed_batch_size)
        .enumerate()
        .map(|(ordinal, chunks)| TokenJob {
            ordinal,
            chunks: chunks.to_vec(),
        })
        .collect())
}

fn chunks_for_text(document: &TextDocument, policy: ChunkPolicy) -> Result<Vec<String>, TextError> {
    let ChunkPolicy::Tokens { max, overlap } = policy else {
        return Ok(vec![document.text.clone()]);
    };
    let words = document.text.split_whitespace().collect::<Vec<_>>();
    if words.len() <= max {
        return Ok(vec![document.text.clone()]);
    }
    let mut chunks = Vec::new();
    let mut start = 0_usize;
    while start < words.len() {
        let end = start.saturating_add(max).min(words.len());
        let chunk = words
            .get(start..end)
            .ok_or(TextError::InvalidInput("chunk range is invalid"))?
            .join(" ");
        chunks.push(chunk);
        if end == words.len() {
            break;
        }
        start = end.saturating_sub(overlap);
    }
    Ok(chunks)
}

fn tokenize_job(
    tokenizer: &crate::tokenizer::ModelTokenizer,
    job: TokenJob,
    max_tokens: usize,
) -> Result<TokenizedJob, TextError> {
    let texts = job
        .chunks
        .iter()
        .map(|chunk| chunk.text.clone())
        .collect::<Vec<_>>();
    let tokens = tokenizer.encode_batch(&texts, max_tokens)?;
    let token_count = tokens.token_ids.len();
    Ok(TokenizedJob {
        ordinal: job.ordinal,
        chunks: job.chunks,
        tokens,
        token_count,
    })
}

fn append_embedded(
    job_chunks: &[ChunkRow],
    embedded: EmbeddingBatch,
    output: &mut Vec<IngestDocument>,
    chunks: &mut usize,
) -> Result<(), TextError> {
    if embedded.rows() != job_chunks.len() {
        return Err(TextError::DimsMismatch {
            declared: u32::try_from(job_chunks.len()).unwrap_or(u32::MAX),
            actual: embedded.rows(),
        });
    }
    let dims = embedded.dims();
    let values = embedded.into_values();
    for (row, chunk) in job_chunks.iter().enumerate() {
        let start = row
            .checked_mul(dims)
            .ok_or(TextError::InvalidInput("embedding row offset overflow"))?;
        let end = start
            .checked_add(dims)
            .ok_or(TextError::InvalidInput("embedding row end overflow"))?;
        let vector = values
            .get(start..end)
            .ok_or(TextError::InvalidInput("embedding row is truncated"))?
            .to_vec();
        output.push(
            IngestDocument::new(
                DocumentVersion::new(
                    encode_chunk_id(chunk.id, chunk.chunk)?,
                    Revision::new(chunk.revision),
                ),
                vector,
            )
            .with_text(chunk.text.clone()),
        );
        *chunks = chunks.saturating_add(1);
    }
    Ok(())
}

fn normalize_batch(
    batch: &mut EmbeddingBatch,
    tower: &zeppelin_embed::epoch::EmbeddingTower,
) -> Result<(), TextError> {
    if batch.dims() != tower.dims as usize {
        return Err(TextError::DimsMismatch {
            declared: tower.dims,
            actual: batch.dims(),
        });
    }
    let dims = batch.dims();
    for row in batch.values_mut().chunks_exact_mut(dims) {
        if tower.normalization == Normalization::L2 {
            let squared_norm = row.iter().try_fold(0.0_f64, |sum, value| {
                value
                    .is_finite()
                    .then_some(sum + f64::from(*value) * f64::from(*value))
            });
            let norm = squared_norm.filter(|value| *value > 0.0).map(f64::sqrt);
            let Some(norm) = norm else {
                return Err(TextError::NonUnitVector);
            };
            for value in row {
                *value = (f64::from(*value) / norm) as f32;
            }
        }
    }
    for row in batch.values().chunks_exact(dims) {
        let norm = row
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum::<f64>();
        if tower.normalization == Normalization::L2 && (norm - 1.0).abs() > 1.0e-5 {
            return Err(TextError::NonUnitVector);
        }
    }
    Ok(())
}

fn encode_chunk_id(id: u128, chunk: u32) -> Result<DocId, TextError> {
    if id >> CALLER_BITS != 0 {
        return Err(TextError::InvalidInput(
            "caller document id exceeds 96 bits",
        ));
    }
    Ok(DocId::new((id << CHUNK_BITS) | u128::from(chunk)))
}

/// Finds the compiled CoreML query tower that belongs to a bundle.
///
/// The Apple Neural Engine is the default query backend, so a compiled
/// `<bundle stem>.mlmodelc` sitting beside the `.zem` is used without
/// asking. `MLModel` refuses a raw `.mlpackage`, so only a compiled
/// directory counts. `ZE_QUERY_COREML` overrides the path and
/// `ZE_QUERY_COREML_TOKENS` its exported sequence length;
/// `ZE_QUERY_COREML=off` forces MLX.
fn discover_query_coreml(bundle_path: &std::path::Path) -> Option<(std::path::PathBuf, usize)> {
    let tokens = std::env::var("ZE_QUERY_COREML_TOKENS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_COREML_TOKENS);
    let model = match std::env::var("ZE_QUERY_COREML") {
        Ok(value) if value == "off" => return None,
        Ok(value) => std::path::PathBuf::from(value),
        Err(_) => bundle_path.with_extension("mlmodelc"),
    };
    model.is_dir().then_some((model, tokens))
}

fn load_versions(store: &Store) -> Result<BTreeMap<DocId, Revision>, TextError> {
    let snapshot = store.snapshot().map_err(TextError::Store)?;
    let mut versions = BTreeMap::new();
    for segment in snapshot.segments() {
        let alive = segment.alive().map_err(|error| {
            TextError::Store(zeppelin_embed::lifecycle::StoreError::Segment(error))
        })?;
        for row in alive.iter_alive() {
            let version = segment.document_version(row as usize).map_err(|error| {
                TextError::Store(zeppelin_embed::lifecycle::StoreError::Segment(error))
            })?;
            if let Some(version) = version {
                versions.insert(version.doc_id(), version.revision());
            }
        }
    }
    Ok(versions)
}

fn stage_start() -> Option<Instant> {
    zeppelin_embed::diag::QUERY_TIMING_ENABLED.then(Instant::now)
}

fn stage_elapsed(started: Option<Instant>) -> Duration {
    started.map_or(Duration::ZERO, |started| started.elapsed())
}

struct RuntimeEmbedding {
    batch: EmbeddingBatch,
    queue_wait: Duration,
    evaluation: Duration,
}

enum RuntimeCommand {
    Embed {
        role: TowerRole,
        tokens: TokenBatch,
        inject_panic: bool,
        queued: Option<Instant>,
        reply: mpsc::SyncSender<Result<RuntimeEmbedding, RuntimeError>>,
    },
    Close,
}

/// Which backend evaluates the query tower.
///
/// CoreML on the Apple Neural Engine is the default when a compiled
/// model sits beside the bundle: at batch one the tower is bound by
/// Metal dispatch count, so CoreML is far faster and far steadier while
/// returning the same vectors. MLX is used when no compiled model is
/// present.
enum QueryRuntime {
    Mlx(MlxRuntime),
    #[cfg(target_os = "macos")]
    CoreMl(Box<crate::runtime::coreml::CoreMlRuntime>),
}

impl QueryRuntime {
    fn backend(&self) -> QueryBackend {
        match self {
            Self::Mlx(runtime) => mlx_backend(runtime),
            #[cfg(target_os = "macos")]
            Self::CoreMl(runtime) => QueryBackend {
                runtime: runtime.identity(),
                requested_compute_units: zeppelin_embed::epoch::ComputeUnits::CpuAndNeuralEngine,
                observed_compute_units: None,
                sequence_length: Some(runtime.sequence()),
            },
        }
    }

    fn embed(&mut self, tokens: &TokenBatch) -> Result<EmbeddingBatch, RuntimeError> {
        match self {
            Self::Mlx(runtime) => runtime.embed_batch(tokens),
            #[cfg(target_os = "macos")]
            Self::CoreMl(runtime) => {
                // A CoreML program is exported at one fixed sequence
                // length, so the batch is padded to it exactly.
                let padded = tokens.padded_to(runtime.sequence())?;
                runtime.embed_batch(&padded)
            }
        }
    }
}

enum RuntimeSet {
    Symmetric(MlxRuntime),
    Pair {
        document: MlxRuntime,
        query: QueryRuntime,
    },
}

impl RuntimeSet {
    fn query_backend(&self) -> QueryBackend {
        match self {
            Self::Symmetric(runtime) => mlx_backend(runtime),
            Self::Pair { query, .. } => query.backend(),
        }
    }

    fn embed(
        &mut self,
        role: TowerRole,
        tokens: &TokenBatch,
    ) -> Result<EmbeddingBatch, RuntimeError> {
        match self {
            Self::Symmetric(runtime) => runtime.embed_batch(tokens),
            Self::Pair { document, query } => match role {
                TowerRole::Document => document.embed_batch(tokens),
                TowerRole::Query => query.embed(tokens),
            },
        }
    }
}

fn mlx_backend(runtime: &MlxRuntime) -> QueryBackend {
    let identity = runtime.identity();
    QueryBackend {
        runtime: identity,
        requested_compute_units: if identity.gpu {
            zeppelin_embed::epoch::ComputeUnits::CpuAndGpu
        } else {
            zeppelin_embed::epoch::ComputeUnits::Cpu
        },
        observed_compute_units: None,
        sequence_length: None,
    }
}

struct RuntimeClient {
    query_backend: QueryBackend,
    sender: mpsc::SyncSender<RuntimeCommand>,
    thread: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl RuntimeClient {
    fn start(
        bundle: Arc<Bundle>,
        query_coreml: Option<(std::path::PathBuf, usize)>,
    ) -> Result<Self, TextError> {
        let (sender, receiver) = mpsc::sync_channel::<RuntimeCommand>(2);
        let (init_sender, init_receiver) = mpsc::sync_channel(0);
        let thread = std::thread::Builder::new()
            .name("ze-text-embed".to_owned())
            .spawn(move || {
                let runtimes = MlxRuntime::load(Arc::clone(&bundle), TowerRole::Document).and_then(
                    |document| {
                        if bundle.is_symmetric() && query_coreml.is_none() {
                            return Ok(RuntimeSet::Symmetric(document));
                        }
                        let dims = bundle.query_tower().embedding.dims as usize;
                        let query = match query_coreml {
                            // A model that was found but will not load is a
                            // broken artifact, not a reason to quietly run
                            // something else.
                            #[cfg(target_os = "macos")]
                            Some((path, sequence)) => crate::runtime::coreml::CoreMlRuntime::load(
                                &path,
                                sequence,
                                dims,
                                crate::runtime::coreml::ComputeUnits::CpuAndNeuralEngine,
                            )
                            .map(|runtime| QueryRuntime::CoreMl(Box::new(runtime)))?,
                            #[cfg(not(target_os = "macos"))]
                            Some(_) => {
                                return Err(RuntimeError::Mlx(
                                    "CoreML is available on macOS only".to_owned(),
                                ));
                            }
                            None => MlxRuntime::load(Arc::clone(&bundle), TowerRole::Query)
                                .map(QueryRuntime::Mlx)?,
                        };
                        Ok(RuntimeSet::Pair { document, query })
                    },
                );
                let mut runtimes = match runtimes {
                    Ok(runtimes) => {
                        let _ = init_sender.send(Ok(runtimes.query_backend()));
                        runtimes
                    }
                    Err(error) => {
                        let _ = init_sender.send(Err(error));
                        return;
                    }
                };
                while let Ok(command) = receiver.recv() {
                    match command {
                        RuntimeCommand::Embed {
                            role,
                            tokens,
                            inject_panic,
                            queued,
                            reply,
                        } => {
                            let queue_wait = stage_elapsed(queued);
                            let evaluation_started = stage_start();
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    if inject_panic {
                                        std::panic::resume_unwind(Box::new("injected embed panic"));
                                    }
                                    runtimes.embed(role, &tokens)
                                }))
                                .unwrap_or(Err(RuntimeError::WorkerPanicked));
                            let evaluation = stage_elapsed(evaluation_started);
                            let result = result.map(|batch| RuntimeEmbedding {
                                batch,
                                queue_wait,
                                evaluation,
                            });
                            let _ = reply.send(result);
                        }
                        RuntimeCommand::Close => break,
                    }
                }
            })
            .map_err(|error| TextError::Pipeline {
                stage: "embed start",
                detail: error.to_string(),
            })?;
        let query_backend = init_receiver.recv().map_err(|_| TextError::Pipeline {
            stage: "embed initialization",
            detail: "thread closed before reporting initialization".to_owned(),
        })??;
        Ok(Self {
            query_backend,
            sender,
            thread: Mutex::new(Some(thread)),
            closed: AtomicBool::new(false),
        })
    }

    fn embed(
        &self,
        role: TowerRole,
        tokens: TokenBatch,
        inject_panic: bool,
    ) -> Result<EmbeddingBatch, TextError> {
        self.embed_with_timing(role, tokens, inject_panic)
            .map(|result| result.batch)
    }

    fn embed_with_timing(
        &self,
        role: TowerRole,
        tokens: TokenBatch,
        inject_panic: bool,
    ) -> Result<RuntimeEmbedding, TextError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TextError::Pipeline {
                stage: "embed submit",
                detail: "runtime is closed".to_owned(),
            });
        }
        let (reply, receive) = mpsc::sync_channel(0);
        self.sender
            .send(RuntimeCommand::Embed {
                role,
                tokens,
                inject_panic,
                queued: stage_start(),
                reply,
            })
            .map_err(|_| TextError::Pipeline {
                stage: "embed submit",
                detail: "runtime channel closed early".to_owned(),
            })?;
        receive
            .recv()
            .map_err(|_| TextError::Pipeline {
                stage: "embed receive",
                detail: "runtime channel closed early".to_owned(),
            })?
            .map_err(|error| match error {
                RuntimeError::WorkerPanicked => TextError::Pipeline {
                    stage: "embed worker",
                    detail: error.to_string(),
                },
                other => TextError::Runtime(other),
            })
    }

    fn close(&self) -> Result<(), TextError> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let _ = self.sender.send(RuntimeCommand::Close);
        let thread = self
            .thread
            .lock()
            .map_err(|_| TextError::Pipeline {
                stage: "embed close",
                detail: "join mutex poisoned".to_owned(),
            })?
            .take();
        if let Some(thread) = thread {
            thread.join().map_err(|_| TextError::Pipeline {
                stage: "embed close",
                detail: "thread panicked".to_owned(),
            })?;
        }
        Ok(())
    }
}

impl Drop for RuntimeClient {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use zeppelin_embed::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower};

    fn tower(dims: u32, normalization: Normalization) -> EmbeddingTower {
        EmbeddingTower {
            model_id: "test".to_owned(),
            model_version: "v1".to_owned(),
            weights_digest: vec![1],
            dims,
            normalization,
            prompt_prefix: String::new(),
            max_tokens: 8,
            runtime: EmbeddingRuntime::Mlx,
            compute_units: ComputeUnits::CpuAndGpu,
            os_build: None,
        }
    }

    #[test]
    fn ingest_options_chunking_and_caller_id_boundaries_fail_loudly() {
        let document = TextDocument::new(7, 1, "one two three four five");
        assert!(validate_ingest_options(&[], IngestOptions::default()).is_err());
        assert!(
            validate_ingest_options(
                std::slice::from_ref(&document),
                IngestOptions {
                    embed_batch_size: 0,
                    ..Default::default()
                },
            )
            .is_err()
        );
        assert!(
            validate_ingest_options(
                std::slice::from_ref(&document),
                IngestOptions {
                    chunk_policy: ChunkPolicy::Tokens { max: 2, overlap: 2 },
                    ..Default::default()
                },
            )
            .is_err()
        );
        assert!(
            validate_ingest_options(
                &[TextDocument::new(1_u128 << CALLER_BITS, 1, "too large")],
                IngestOptions::default(),
            )
            .is_err()
        );
        let chunks = chunks_for_text(&document, ChunkPolicy::Tokens { max: 3, overlap: 1 })
            .expect("overlap chunks");
        assert_eq!(chunks, ["one two three", "three four five"]);
        assert_eq!(
            chunks_for_text(&document, ChunkPolicy::None).expect("no chunks"),
            [document.text.clone()]
        );
        assert!(
            build_jobs(
                std::slice::from_ref(&document),
                IngestOptions {
                    chunk_policy: ChunkPolicy::None,
                    ..Default::default()
                },
                3,
            )
            .is_err()
        );
        assert_eq!(
            encode_chunk_id(7, 3).expect("chunk id").get(),
            (7_u128 << 32) | 3
        );
        assert!(encode_chunk_id(1_u128 << CALLER_BITS, 0).is_err());
    }

    #[test]
    fn vector_normalization_checks_dimensions_finiteness_and_unit_norm() {
        let mut batch = EmbeddingBatch::new(vec![3.0, 4.0], 1, 2).expect("batch");
        normalize_batch(&mut batch, &tower(2, Normalization::L2)).expect("normalize");
        assert_eq!(batch.values(), [0.6, 0.8]);
        let mut unchanged = EmbeddingBatch::new(vec![3.0, 4.0], 1, 2).expect("batch");
        normalize_batch(&mut unchanged, &tower(2, Normalization::None)).expect("no normalize");
        assert_eq!(unchanged.values(), [3.0, 4.0]);
        assert!(normalize_batch(&mut unchanged, &tower(3, Normalization::None)).is_err());
        let mut zero = EmbeddingBatch::new(vec![0.0, 0.0], 1, 2).expect("zero batch");
        assert!(normalize_batch(&mut zero, &tower(2, Normalization::L2)).is_err());
        let mut nan = EmbeddingBatch::new(vec![f32::NAN, 1.0], 1, 2).expect("nan batch");
        assert!(normalize_batch(&mut nan, &tower(2, Normalization::L2)).is_err());
    }

    #[test]
    fn cancellation_prevents_acquiring_an_in_flight_slot() {
        let control = IngestControl::new();
        control.cancel();
        assert_eq!(acquire_buffer_slot(&AtomicUsize::new(0), 1, &control), None);
        assert_eq!(
            IngestControl::default().fault_fired(TextFaultSite::EmbedWorkerPanic),
            false
        );
    }
}
