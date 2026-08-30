use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::campaign::{CampaignKind, FaultPlan, FeatureFaultEvent};
use super::coverage::CoverageRegistry;
use super::fault_vfs::{FaultEvent, plan_schedule};
use super::oracle::OracleRecord;
use super::profiles::{FaultProfile, environment_for_profile};
use super::program::Program;
use super::runner::ComparisonOutcomeCounts;
use super::runner::Violation;

pub const REPLAY_ARTIFACTS: [&str; 9] = [
    "program.jsonl",
    "faults.jsonl",
    "violations.json",
    "coverage.json",
    "oracle.jsonl",
    "controls.jsonl",
    "receipts.jsonl",
    "mutations.jsonl",
    "episode.json",
];
pub const STORAGE_REPLAY_ARTIFACTS: [&str; 6] = [
    "storage-fixture.json",
    "ack-ledger.jsonl",
    "storage-observations.jsonl",
    "feature-receipts.jsonl",
    "clean-controls.jsonl",
    "artifact-index.jsonl",
];
pub const METADATA_REPLAY_ARTIFACTS: [&str; 3] = [
    "metadata-fixture.json",
    "queries.jsonl",
    "fixture-mutations.jsonl",
];
pub const VECTOR_REPLAY_ARTIFACTS: [&str; 8] = [
    "fixture.json",
    "backend-inventory.json",
    "quantization.jsonl",
    "rescore.jsonl",
    "identity.jsonl",
    "coverage.jsonl",
    "violations.jsonl",
    "episode-summary.json",
];
pub const INGEST_REPLAY_ARTIFACTS: [&str; 2] = ["fixture.json", "observations.jsonl"];

#[must_use]
pub fn replay_artifacts_for(campaign: CampaignKind) -> Vec<&'static str> {
    let mut artifacts = REPLAY_ARTIFACTS.to_vec();
    if campaign == CampaignKind::StorageDurability {
        artifacts.extend(STORAGE_REPLAY_ARTIFACTS);
    }
    if campaign == CampaignKind::MetadataFilterPlanner {
        artifacts.extend(METADATA_REPLAY_ARTIFACTS);
    }
    if campaign == CampaignKind::VectorExecution {
        artifacts.extend(VECTOR_REPLAY_ARTIFACTS);
    }
    if campaign == CampaignKind::IngestRetention {
        artifacts.extend(INGEST_REPLAY_ARTIFACTS);
    }
    artifacts
}

pub fn verify_recomputed_fault_plan_bytes(
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    faults_bytes: &[u8],
) -> Result<FaultPlan, String> {
    let records = faults_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .map_err(|error| format!("parse faults.jsonl row: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let plan = verify_recomputed_fault_plan_records(campaign, seed, profile, &records)?;
    let recomputed = fault_plan_bytes(&plan.schedule.events, &plan.feature);
    if recomputed != faults_bytes {
        return Err("recomputed fault plan does not equal faults.jsonl bytes".to_owned());
    }
    Ok(plan)
}

pub fn verify_recomputed_fault_plan_records(
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    records: &[zeppelin_embed_bench::harness_json::Value],
) -> Result<FaultPlan, String> {
    let program = Program::generate_for(campaign, seed);
    let schedule = plan_schedule(seed, environment_for_profile(profile, seed), &program);
    let mut plan = FaultPlan::for_program(campaign, seed, profile, &program, schedule);
    let expected_records = plan
        .schedule
        .events
        .len()
        .saturating_add(plan.feature.len());
    if records.len() != expected_records {
        return Err(format!(
            "faults.jsonl record count expected={expected_records} observed={}",
            records.len()
        ));
    }

    for (event, record) in plan.schedule.events.iter_mut().zip(records) {
        if record["type"].as_str() != Some("generic") {
            return Err("faults.jsonl generic plan row has the wrong type".to_owned());
        }
        let fired = record["fired"]
            .as_bool()
            .ok_or_else(|| "faults.jsonl generic row omitted fired".to_owned())?;
        let fire_count = record["fire_count"]
            .as_u64()
            .ok_or_else(|| "faults.jsonl generic row omitted fire_count".to_owned())?;
        if fire_count != u64::from(fired) {
            return Err(format!(
                "faults.jsonl generic row has fired={fired} fire_count={fire_count}"
            ));
        }
        event.fired = fired;
        event.fire_count = usize::from(fired);
        event.path = match record.get("path") {
            Some(value) if value.is_null() => None,
            Some(value) => Some(PathBuf::from(value.as_str().ok_or_else(|| {
                "faults.jsonl generic path is neither string nor null".to_owned()
            })?)),
            None => return Err("faults.jsonl generic row omitted path".to_owned()),
        };
    }
    for (event, record) in plan
        .feature
        .iter_mut()
        .zip(records.iter().skip(plan.schedule.events.len()))
    {
        if record["type"].as_str() != Some("feature") {
            return Err("faults.jsonl feature plan row has the wrong type".to_owned());
        }
        let fired = record["fired"]
            .as_bool()
            .ok_or_else(|| "faults.jsonl feature row omitted fired".to_owned())?;
        let fire_count = record["fire_count"]
            .as_u64()
            .ok_or_else(|| "faults.jsonl feature row omitted fire_count".to_owned())?;
        let expected_fire_count = if fired {
            u64::try_from(event.fault.required_receipt_cardinality())
                .map_err(|_| "feature receipt cardinality exceeds u64".to_owned())?
        } else {
            0
        };
        if fire_count != expected_fire_count {
            return Err(format!(
                "faults.jsonl feature row has fired={fired} fire_count={fire_count} expected={expected_fire_count}"
            ));
        }
        event.fired = fired;
        event.fire_count = usize::try_from(fire_count)
            .map_err(|_| "feature fire_count exceeds usize".to_owned())?;
    }

    let recomputed = fault_plan_bytes(&plan.schedule.events, &plan.feature)
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_slice::<
                zeppelin_embed_bench::harness_json::Value,
            >(line)
            .map_err(|error| format!("parse recomputed faults.jsonl row: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if recomputed != records {
        return Err("recomputed fault plan does not equal faults.jsonl records".to_owned());
    }
    Ok(plan)
}

fn fault_plan_bytes(faults: &[FaultEvent], feature_faults: &[FeatureFaultEvent]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for fault in faults {
        bytes.extend_from_slice(fault.json_line().as_bytes());
        bytes.push(b'\n');
    }
    for fault in feature_faults {
        bytes.extend_from_slice(fault.json_line().as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

#[must_use]
pub const fn oracle_contract(campaign: CampaignKind) -> &'static str {
    match campaign {
        CampaignKind::StorageDurability => {
            zeppelin_embed_adversarial_oracle::storage_durability::ORACLE_CONTRACT_VERSION
        }
        CampaignKind::VectorExecution => {
            zeppelin_embed_adversarial_oracle::vector_execution::VECTOR_ORACLE_CONTRACT
        }
        CampaignKind::MetadataFilterPlanner => "metadata-filter-planner-oracle-v2",
        CampaignKind::Overall => "overall-oracle-v1",
        CampaignKind::IngestRetention => {
            zeppelin_embed_adversarial_oracle::ingest_retention::ORACLE_CONTRACT_VERSION
        }
        CampaignKind::VamanaGraph => "vamana-graph-oracle-v1",
        CampaignKind::Fts => "fts-oracle-v1",
        CampaignKind::HybridFusion => "hybrid-fusion-oracle-v1",
        CampaignKind::TieringMaintenance => "tiering-maintenance-oracle-v1",
        CampaignKind::LifecycleAccounting => "lifecycle-accounting-oracle-v1",
        CampaignKind::DiagnosticsHealth => "diagnostics-health-oracle-v1",
        CampaignKind::FfiBindings => "ffi-bindings-oracle-v1",
    }
}
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpisodeAttestation {
    pub comparison_counts: BTreeMap<String, u64>,
    pub comparison_outcome_counts: BTreeMap<String, ComparisonOutcomeCounts>,
    pub same_seed_clean_controls: u64,
    pub integrated_feature_fault_receipts: u64,
    pub expected_feature_fault_receipts: u64,
    pub evidence_digests: BTreeMap<String, String>,
    /// Family-owned strict attestation nested under the campaign-specific key.
    pub family_oracle_attestation: Option<zeppelin_embed_bench::harness_json::Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedStreamStats {
    pub records: u64,
    pub bytes: u64,
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedEvidenceStats {
    pub episodes: u64,
    pub streams: BTreeMap<String, MergedStreamStats>,
}

pub struct MergedEvidence {
    root: PathBuf,
    program: MergedStream,
    faults: MergedStream,
    violations: MergedStream,
    coverage: MergedStream,
    oracle: MergedStream,
    controls: MergedStream,
    receipts: MergedStream,
    mutations: MergedStream,
    family: BTreeMap<String, MergedStream>,
    index: File,
    episodes: u64,
    episode_keys: BTreeSet<(u64, String)>,
}

struct MergedStream {
    name: String,
    path: PathBuf,
    file: File,
    records: u64,
    bytes: u64,
    digest_state: u64,
}

impl MergedEvidence {
    pub fn create(root: &Path) -> Result<Self, String> {
        fs::create_dir_all(root)
            .map_err(|error| format!("create merged evidence root: {error}"))?;
        let mut evidence = Self {
            root: root.to_path_buf(),
            program: MergedStream::create(root, "program")?,
            faults: MergedStream::create(root, "faults")?,
            violations: MergedStream::create(root, "violations")?,
            coverage: MergedStream::create(root, "coverage")?,
            oracle: MergedStream::create(root, "oracle")?,
            controls: MergedStream::create(root, "controls")?,
            receipts: MergedStream::create(root, "receipts")?,
            mutations: MergedStream::create(root, "mutations")?,
            family: BTreeMap::new(),
            index: create_merged_file(root, "merged-index.jsonl")?,
            episodes: 0,
            episode_keys: BTreeSet::new(),
        };
        evidence.sync_all()?;
        Ok(evidence)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "merged evidence atomically binds every attested episode stream"
    )]
    pub fn append_episode(
        &mut self,
        seed: u64,
        profile: FaultProfile,
        program: &[u8],
        faults: &[u8],
        violations: &[u8],
        coverage: &[u8],
        oracle: &[u8],
        controls: &[u8],
        receipts: &[u8],
        mutations: &[u8],
        family: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(), String> {
        let episode_key = (seed, profile.key().to_owned());
        if self.episode_keys.contains(&episode_key) {
            return Err(format!(
                "duplicate merged episode seed={seed} profile={}",
                profile.key()
            ));
        }
        validate_oracle_identities(seed, profile, oracle)?;
        let program_episode = self.program.append(seed, profile, program)?;
        let faults_episode = self.faults.append(seed, profile, faults)?;
        let violations_episode = self
            .violations
            .append_json_document(seed, profile, violations)?;
        let coverage_episode = self.coverage.append(seed, profile, coverage)?;
        let oracle_episode = self.oracle.append(seed, profile, oracle)?;
        let controls_episode = self.controls.append(seed, profile, controls)?;
        let receipts_episode = self.receipts.append(seed, profile, receipts)?;
        let mutations_episode = self.mutations.append(seed, profile, mutations)?;
        let existing_family = self.family.keys().collect::<Vec<_>>();
        let incoming_family = family.keys().collect::<Vec<_>>();
        if self.episodes != 0 && existing_family != incoming_family {
            return Err(format!(
                "merged family evidence stream set changed: existing={existing_family:?} incoming={incoming_family:?}"
            ));
        }
        let mut family_episodes = BTreeMap::new();
        for (name, source) in family {
            if !self.family.contains_key(name) {
                let stream_name = format!("family-{name}");
                let stream = MergedStream::create(&self.root, &stream_name)?;
                self.family.insert(name.clone(), stream);
            }
            let stream = self
                .family
                .get_mut(name)
                .ok_or_else(|| format!("merged family stream {name} disappeared"))?;
            let episode = if name.ends_with(".json") {
                stream.append_json_document(seed, profile, source)?
            } else {
                stream.append(seed, profile, source)?
            };
            family_episodes.insert(name.clone(), episode);
        }
        self.program.sync()?;
        self.faults.sync()?;
        self.violations.sync()?;
        self.coverage.sync()?;
        self.oracle.sync()?;
        self.controls.sync()?;
        self.receipts.sync()?;
        self.mutations.sync()?;
        for stream in self.family.values() {
            stream.sync()?;
        }
        self.program.verify_tail(seed, profile, &program_episode)?;
        self.faults.verify_tail(seed, profile, &faults_episode)?;
        self.violations
            .verify_tail(seed, profile, &violations_episode)?;
        self.coverage
            .verify_tail(seed, profile, &coverage_episode)?;
        self.oracle.verify_tail(seed, profile, &oracle_episode)?;
        self.controls
            .verify_tail(seed, profile, &controls_episode)?;
        self.receipts
            .verify_tail(seed, profile, &receipts_episode)?;
        self.mutations
            .verify_tail(seed, profile, &mutations_episode)?;
        for (name, episode) in &family_episodes {
            self.family
                .get(name)
                .ok_or_else(|| format!("merged family stream {name} disappeared"))?
                .verify_tail(seed, profile, episode)?;
        }

        let mut streams = BTreeMap::from([
            ("program".to_owned(), program_episode),
            ("faults".to_owned(), faults_episode),
            ("violations".to_owned(), violations_episode),
            ("coverage".to_owned(), coverage_episode),
            ("oracle".to_owned(), oracle_episode),
            ("controls".to_owned(), controls_episode),
            ("receipts".to_owned(), receipts_episode),
            ("mutations".to_owned(), mutations_episode),
        ]);
        streams.extend(
            family_episodes
                .into_iter()
                .map(|(name, value)| (format!("family/{name}"), value)),
        );

        let mut index = zeppelin_embed_bench::harness_json::to_vec(
            &zeppelin_embed_bench::harness_json::json!({
                "campaign_episode": self.episodes,
                "seed": seed,
                "profile": profile.key(),
                "streams": streams,
            }),
        )
        .map_err(|error| format!("serialize merged evidence index: {error}"))?;
        index.push(b'\n');
        self.index
            .write_all(&index)
            .map_err(|error| format!("append merged-index.jsonl: {error}"))?;
        self.index
            .sync_all()
            .map_err(|error| format!("sync merged-index.jsonl: {error}"))?;
        self.verify_index_tail(&index)?;
        File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("sync merged evidence directory: {error}"))?;
        self.episodes = self.episodes.saturating_add(1);
        self.episode_keys.insert(episode_key);
        Ok(())
    }

    /// Reopens every merged stream and verifies exact length, JSON framing,
    /// record count, and aggregate digest before evidence may qualify.
    pub fn verify_durable(&self) -> Result<(), String> {
        for stream in [
            &self.program,
            &self.faults,
            &self.violations,
            &self.coverage,
            &self.oracle,
            &self.controls,
            &self.receipts,
            &self.mutations,
        ] {
            stream.verify_durable()?;
        }
        for stream in self.family.values() {
            stream.verify_durable()?;
        }
        let path = self.root.join("merged-index.jsonl");
        let bytes = fs::read(&path)
            .map_err(|error| format!("merged-index.jsonl reopen verification read: {error}"))?;
        let mut records = 0_u64;
        let mut episode_keys = BTreeSet::new();
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line).map_err(|error| {
                    format!("merged-index.jsonl reopen verification parse: {error}")
                })?;
            let seed = record["seed"]
                .as_u64()
                .ok_or_else(|| "merged-index.jsonl reopen verification omitted seed".to_owned())?;
            let profile = record["profile"].as_str().ok_or_else(|| {
                "merged-index.jsonl reopen verification omitted profile".to_owned()
            })?;
            if !episode_keys.insert((seed, profile.to_owned())) {
                return Err(format!(
                    "merged-index.jsonl reopen verification found duplicate merged episode seed={seed} profile={profile}"
                ));
            }
            records = records.saturating_add(1);
        }
        if records != self.episodes {
            return Err(format!(
                "merged-index.jsonl reopen verification record count expected={} observed={records}",
                self.episodes
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn stats(&self) -> MergedEvidenceStats {
        let mut streams = [
            self.program.stats(),
            self.faults.stats(),
            self.violations.stats(),
            self.coverage.stats(),
            self.oracle.stats(),
            self.controls.stats(),
            self.receipts.stats(),
            self.mutations.stats(),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        streams.extend(
            self.family
                .iter()
                .map(|(name, stream)| (format!("family/{name}"), stream.stats().1)),
        );
        MergedEvidenceStats {
            episodes: self.episodes,
            streams,
        }
    }

    fn sync_all(&mut self) -> Result<(), String> {
        self.program.sync()?;
        self.faults.sync()?;
        self.violations.sync()?;
        self.coverage.sync()?;
        self.oracle.sync()?;
        self.controls.sync()?;
        self.receipts.sync()?;
        self.mutations.sync()?;
        for stream in self.family.values() {
            stream.sync()?;
        }
        self.index
            .sync_all()
            .map_err(|error| format!("sync merged-index.jsonl: {error}"))?;
        File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("sync merged evidence directory: {error}"))
    }

    fn verify_index_tail(&self, expected: &[u8]) -> Result<(), String> {
        let path = self.root.join("merged-index.jsonl");
        let mut file = File::open(&path)
            .map_err(|error| format!("merged-index.jsonl reopen verification open: {error}"))?;
        let length = file
            .metadata()
            .map_err(|error| format!("merged-index.jsonl reopen verification metadata: {error}"))?
            .len();
        let expected_length = u64::try_from(expected.len())
            .map_err(|_| "merged-index.jsonl expected tail exceeds u64".to_owned())?;
        let start = length
            .checked_sub(expected_length)
            .ok_or_else(|| "merged-index.jsonl reopen verification tail underflowed".to_owned())?;
        file.seek(SeekFrom::Start(start))
            .map_err(|error| format!("merged-index.jsonl reopen verification seek: {error}"))?;
        let mut observed = vec![0_u8; expected.len()];
        file.read_exact(&mut observed)
            .map_err(|error| format!("merged-index.jsonl reopen verification read: {error}"))?;
        if observed != expected {
            return Err("merged-index.jsonl reopen verification tail differs".to_owned());
        }
        let line = observed.strip_suffix(b"\n").unwrap_or(&observed);
        let _: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line).map_err(|error| {
                format!("merged-index.jsonl reopen verification parse: {error}")
            })?;
        Ok(())
    }
}

fn validate_oracle_identities(
    seed: u64,
    profile: FaultProfile,
    oracle: &[u8],
) -> Result<(), String> {
    let mut identities = BTreeSet::new();
    for line in oracle
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse oracle evidence row preflight: {error}"))?;
        let Some(checker_id) = record["checker_id"].as_str() else {
            continue;
        };
        let query_identity = record["case_identity"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                record["expected"]["query_id"]
                    .as_u64()
                    .map(|query_id| format!("query-{query_id}"))
            })
            .or_else(|| {
                record["expected"]["case_id"]
                    .as_u64()
                    .map(|case_id| format!("case-{case_id}"))
            })
            .or_else(|| record["oracle_input_digest"].as_str().map(str::to_owned))
            .or_else(|| record["input_digest"].as_str().map(str::to_owned));
        let Some(query_identity) = query_identity else {
            continue;
        };
        if !identities.insert((checker_id.to_owned(), query_identity.clone())) {
            return Err(format!(
                "duplicate merged oracle identity seed={seed} profile={} checker_id={checker_id} query_id={query_identity}",
                profile.key()
            ));
        }
    }
    Ok(())
}

impl MergedStream {
    fn create(root: &Path, name: &str) -> Result<Self, String> {
        let path = root.join(format!("merged-{name}.jsonl"));
        Ok(Self {
            name: name.to_owned(),
            file: create_merged_file(root, &format!("merged-{name}.jsonl"))?,
            path,
            records: 0,
            bytes: 0,
            digest_state: FNV_OFFSET_BASIS,
        })
    }

    fn append(
        &mut self,
        seed: u64,
        profile: FaultProfile,
        source: &[u8],
    ) -> Result<zeppelin_embed_bench::harness_json::Value, String> {
        let mut appended_bytes = 0_u64;
        let mut appended_records = 0_u64;
        let mut appended_digest_state = FNV_OFFSET_BASIS;
        for line in source
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line)
                    .map_err(|error| format!("parse {} evidence row: {error}", self.name))?;
            let mut envelope = zeppelin_embed_bench::harness_json::to_vec(
                &zeppelin_embed_bench::harness_json::json!({
                    "seed": seed,
                    "profile": profile.key(),
                    "record": record,
                }),
            )
            .map_err(|error| format!("serialize {} evidence row: {error}", self.name))?;
            envelope.push(b'\n');
            self.file
                .write_all(&envelope)
                .map_err(|error| format!("append merged-{}.jsonl: {error}", self.name))?;
            update_fnv(&mut self.digest_state, &envelope);
            update_fnv(&mut appended_digest_state, &envelope);
            let length = envelope.len() as u64;
            self.bytes = self.bytes.saturating_add(length);
            self.records = self.records.saturating_add(1);
            appended_bytes = appended_bytes.saturating_add(length);
            appended_records = appended_records.saturating_add(1);
        }
        Ok(zeppelin_embed_bench::harness_json::json!({
            "source_records": source.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).count(),
            "source_bytes": source.len(),
            "source_digest": evidence_digest(&[source]),
            "merged_records": appended_records,
            "merged_bytes": appended_bytes,
            "merged_digest": finish_fnv(appended_digest_state),
        }))
    }

    fn append_json_document(
        &mut self,
        seed: u64,
        profile: FaultProfile,
        source: &[u8],
    ) -> Result<zeppelin_embed_bench::harness_json::Value, String> {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(source)
                .map_err(|error| format!("parse {} evidence document: {error}", self.name))?;
        let mut envelope = zeppelin_embed_bench::harness_json::to_vec(
            &zeppelin_embed_bench::harness_json::json!({
                "seed": seed,
                "profile": profile.key(),
                "record": record,
            }),
        )
        .map_err(|error| format!("serialize {} evidence document: {error}", self.name))?;
        envelope.push(b'\n');
        self.file
            .write_all(&envelope)
            .map_err(|error| format!("append merged-{}.jsonl: {error}", self.name))?;
        update_fnv(&mut self.digest_state, &envelope);
        let length = envelope.len() as u64;
        self.bytes = self.bytes.saturating_add(length);
        self.records = self.records.saturating_add(1);
        Ok(zeppelin_embed_bench::harness_json::json!({
            "source_records": 1,
            "source_bytes": source.len(),
            "source_digest": evidence_digest(&[source]),
            "merged_records": 1,
            "merged_bytes": length,
            "merged_digest": evidence_digest(&[&envelope]),
        }))
    }

    fn sync(&self) -> Result<(), String> {
        self.file
            .sync_all()
            .map_err(|error| format!("sync merged-{}.jsonl: {error}", self.name))
    }

    fn verify_tail(
        &self,
        seed: u64,
        profile: FaultProfile,
        episode: &zeppelin_embed_bench::harness_json::Value,
    ) -> Result<(), String> {
        let merged_bytes = episode["merged_bytes"]
            .as_u64()
            .ok_or_else(|| format!("{} reopen verification omitted merged_bytes", self.label()))?;
        let merged_records = episode["merged_records"].as_u64().ok_or_else(|| {
            format!(
                "{} reopen verification omitted merged_records",
                self.label()
            )
        })?;
        let merged_digest = episode["merged_digest"]
            .as_str()
            .ok_or_else(|| format!("{} reopen verification omitted merged_digest", self.label()))?;
        let start = self.bytes.checked_sub(merged_bytes).ok_or_else(|| {
            format!(
                "{} reopen verification byte range underflowed",
                self.label()
            )
        })?;
        let mut file = File::open(&self.path)
            .map_err(|error| format!("{} reopen verification open: {error}", self.label()))?;
        let actual_length = file
            .metadata()
            .map_err(|error| format!("{} reopen verification metadata: {error}", self.label()))?
            .len();
        if actual_length != self.bytes {
            return Err(format!(
                "{} reopen verification length expected={} observed={actual_length}",
                self.label(),
                self.bytes
            ));
        }
        file.seek(SeekFrom::Start(start))
            .map_err(|error| format!("{} reopen verification seek: {error}", self.label()))?;
        let tail_length = usize::try_from(merged_bytes)
            .map_err(|_| format!("{} reopen verification tail exceeds usize", self.label()))?;
        let mut tail = vec![0_u8; tail_length];
        file.read_exact(&mut tail)
            .map_err(|error| format!("{} reopen verification read: {error}", self.label()))?;
        if evidence_digest(&[&tail]) != merged_digest {
            return Err(format!(
                "{} reopen verification tail digest differs",
                self.label()
            ));
        }
        let mut records = 0_u64;
        for line in tail
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let envelope: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line).map_err(|error| {
                    format!("{} reopen verification parse: {error}", self.label())
                })?;
            if envelope["seed"].as_u64() != Some(seed)
                || envelope["profile"].as_str() != Some(profile.key())
                || envelope.get("record").is_none()
            {
                return Err(format!(
                    "{} reopen verification episode identity differs",
                    self.label()
                ));
            }
            records = records.saturating_add(1);
        }
        if records != merged_records {
            return Err(format!(
                "{} reopen verification record count expected={merged_records} observed={records}",
                self.label()
            ));
        }
        Ok(())
    }

    fn verify_durable(&self) -> Result<(), String> {
        let bytes = fs::read(&self.path)
            .map_err(|error| format!("{} reopen verification read: {error}", self.label()))?;
        let expected_length = usize::try_from(self.bytes)
            .map_err(|_| format!("{} reopen verification length exceeds usize", self.label()))?;
        if bytes.len() != expected_length {
            return Err(format!(
                "{} reopen verification length expected={expected_length} observed={}",
                self.label(),
                bytes.len()
            ));
        }
        let mut records = 0_u64;
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let _: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_slice(line).map_err(|error| {
                    format!("{} reopen verification parse: {error}", self.label())
                })?;
            records = records.saturating_add(1);
        }
        if records != self.records {
            return Err(format!(
                "{} reopen verification record count expected={} observed={records}",
                self.label(),
                self.records
            ));
        }
        if evidence_digest(&[&bytes]) != finish_fnv(self.digest_state) {
            return Err(format!(
                "{} reopen verification aggregate digest differs",
                self.label()
            ));
        }
        Ok(())
    }

    fn label(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("merged evidence stream")
            .to_owned()
    }

    fn stats(&self) -> (String, MergedStreamStats) {
        (
            self.name.clone(),
            MergedStreamStats {
                records: self.records,
                bytes: self.bytes,
                digest: finish_fnv(self.digest_state),
            },
        )
    }
}

fn create_merged_file(root: &Path, name: &str) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(name))
        .map_err(|error| format!("create {name}: {error}"))
}

pub struct RunArtifacts {
    directory: PathBuf,
}

impl RunArtifacts {
    pub fn create(root: &Path, seed: u64, profile: FaultProfile) -> Result<Self, String> {
        let directory = root.join(format!("seed-{seed}-{}", profile.key()));
        fs::create_dir_all(&directory).map_err(|error| {
            format!("create artifact directory {}: {error}", directory.display())
        })?;
        Ok(Self { directory })
    }

    pub fn create_for(
        root: &Path,
        campaign: CampaignKind,
        seed: u64,
        profile: FaultProfile,
    ) -> Result<Self, String> {
        if campaign == CampaignKind::Overall {
            Self::create(root, seed, profile)
        } else {
            Self::create(&root.join(campaign.key()), seed, profile)
        }
    }

    pub fn write_program(&self, program: &Program) -> Result<Vec<u8>, String> {
        let bytes = program.jsonl();
        fs::write(self.directory.join("program.jsonl"), &bytes)
            .map_err(|error| format!("write program.jsonl: {error}"))?;
        Ok(bytes)
    }

    pub fn write_faults(&self, faults: &[FaultEvent]) -> Result<Vec<u8>, String> {
        let bytes = fault_plan_bytes(faults, &[]);
        fs::write(self.directory.join("faults.jsonl"), &bytes)
            .map_err(|error| format!("write faults.jsonl: {error}"))?;
        Ok(bytes)
    }

    pub fn write_fault_plan(
        &self,
        faults: &[FaultEvent],
        feature_faults: &[FeatureFaultEvent],
    ) -> Result<Vec<u8>, String> {
        if feature_faults.is_empty() {
            return self.write_faults(faults);
        }
        let bytes = fault_plan_bytes(faults, feature_faults);
        fs::write(self.directory.join("faults.jsonl"), &bytes)
            .map_err(|error| format!("write faults.jsonl: {error}"))?;
        Ok(bytes)
    }

    pub fn write_violations(&self, violations: &[Violation]) -> Result<Vec<u8>, String> {
        self.write_violations_for(CampaignKind::Overall, violations)
    }

    pub fn write_violations_for(
        &self,
        campaign: CampaignKind,
        violations: &[Violation],
    ) -> Result<Vec<u8>, String> {
        let mut text = String::from("[\n");
        for (index, violation) in violations.iter().enumerate() {
            if index != 0 {
                text.push_str(",\n");
            }
            text.push_str("  ");
            text.push_str(&violation.json_for(campaign));
        }
        text.push_str("\n]\n");
        let bytes = text.into_bytes();
        fs::write(self.directory.join("violations.json"), &bytes)
            .map_err(|error| format!("write violations.json: {error}"))?;
        Ok(bytes)
    }

    pub fn write_reproduction(&self, command: &str) -> Result<(), String> {
        fs::write(self.directory.join("repro.txt"), format!("{command}\n"))
            .map_err(|error| format!("write repro.txt: {error}"))
    }

    pub fn write_episode_metadata(
        &self,
        campaign: CampaignKind,
        seed: u64,
        profile: FaultProfile,
        profile_overridden: bool,
        reproduction: &str,
        attestation: Option<&EpisodeAttestation>,
    ) -> Result<Vec<u8>, String> {
        let metadata = if let Some(attestation) = attestation {
            let mut attestation_json = zeppelin_embed_bench::harness_json::json!({
                "oracle_contract_version": zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION,
                "oracle_contract": oracle_contract(campaign),
                "harness_git_revision": harness_git_revision(),
                "comparison_counts": attestation.comparison_counts,
                "comparison_outcome_counts": attestation.comparison_outcome_counts.iter().map(|(invariant, counts)| {
                    (invariant.clone(), zeppelin_embed_bench::harness_json::json!({
                        "equal": counts.equal,
                        "refused": counts.refused,
                    }))
                }).collect::<BTreeMap<_, _>>(),
                "same_seed_clean_controls": attestation.same_seed_clean_controls,
                "integrated_feature_fault_receipts": attestation.integrated_feature_fault_receipts,
                "expected_feature_fault_receipts": attestation.expected_feature_fault_receipts,
                "evidence_digests": attestation.evidence_digests,
                "replay_artifacts": replay_artifacts_for(campaign),
            });
            if let Some(family) = &attestation.family_oracle_attestation {
                let key = match campaign {
                    CampaignKind::StorageDurability => "storage_oracle_attestation",
                    CampaignKind::VectorExecution => "vector_oracle_attestation",
                    CampaignKind::MetadataFilterPlanner => "metadata_oracle_attestation",
                    CampaignKind::IngestRetention => "ingest_retention_oracle_attestation",
                    _ => "feature_oracle_attestation",
                };
                attestation_json
                    .as_object_mut()
                    .expect("episode attestation is an object")
                    .insert(key.to_owned(), family.clone());
            }
            zeppelin_embed_bench::harness_json::json!({
                "schema": "zeppelin-embed-adversarial-episode",
                "version": 3,
                "campaign": campaign.key(),
                "seed": seed,
                "profile": profile.key(),
                "profile_overridden": profile_overridden,
                "reproduction": reproduction,
                "attestation": attestation_json,
            })
        } else {
            zeppelin_embed_bench::harness_json::json!({
                "schema": "zeppelin-embed-adversarial-episode",
                "version": 3,
                "campaign": campaign.key(),
                "seed": seed,
                "profile": profile.key(),
                "profile_overridden": profile_overridden,
                "reproduction": reproduction,
            })
        };
        let mut bytes = zeppelin_embed_bench::harness_json::to_vec_pretty(&metadata)
            .map_err(|error| format!("serialize episode.json: {error}"))?;
        bytes.push(b'\n');
        fs::write(self.directory.join("episode.json"), &bytes)
            .map_err(|error| format!("write episode.json: {error}"))?;
        Ok(bytes)
    }

    pub fn write_coverage(&self, coverage: &CoverageRegistry) -> Result<Vec<u8>, String> {
        let bytes = coverage.json().into_bytes();
        fs::write(self.directory.join("coverage.json"), &bytes)
            .map_err(|error| format!("write coverage.json: {error}"))?;
        Ok(bytes)
    }

    pub fn write_oracle(&self, records: &[OracleRecord]) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(record.json_line().as_bytes());
            bytes.push(b'\n');
        }
        fs::write(self.directory.join("oracle.jsonl"), &bytes)
            .map_err(|error| format!("write oracle.jsonl: {error}"))?;
        Ok(bytes)
    }

    pub fn write_controls(&self, records: &[String]) -> Result<Vec<u8>, String> {
        self.write_json_lines("controls.jsonl", records)
    }

    pub fn write_receipts(&self, records: &[String]) -> Result<Vec<u8>, String> {
        self.write_json_lines("receipts.jsonl", records)
    }

    pub fn write_mutations(&self, records: &[String]) -> Result<Vec<u8>, String> {
        self.write_json_lines("mutations.jsonl", records)
    }

    pub fn write_family_artifact(
        &self,
        campaign: CampaignKind,
        name: &str,
        bytes: &[u8],
    ) -> Result<Vec<u8>, String> {
        if !replay_artifacts_for(campaign).contains(&name) || REPLAY_ARTIFACTS.contains(&name) {
            return Err(format!("unregistered family replay artifact {name}"));
        }
        let mut file = File::create(self.directory.join(name))
            .map_err(|error| format!("create {name}: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("write {name}: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync {name}: {error}"))?;
        File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("sync family artifact directory: {error}"))?;
        Ok(bytes.to_vec())
    }

    fn write_json_lines(&self, name: &str, records: &[String]) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(record.as_bytes());
            bytes.push(b'\n');
        }
        fs::write(self.directory.join(name), &bytes)
            .map_err(|error| format!("write {name}: {error}"))?;
        Ok(bytes)
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

#[must_use]
pub fn evidence_digest(parts: &[&[u8]]) -> String {
    let mut digest = FNV_OFFSET_BASIS;
    for part in parts {
        update_fnv(&mut digest, part);
        digest ^= 0xff;
        digest = digest.wrapping_mul(FNV_PRIME);
    }
    format!("fnv1a64:{digest:016x}")
}

fn update_fnv(digest: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *digest ^= u64::from(*byte);
        *digest = digest.wrapping_mul(FNV_PRIME);
    }
}

fn finish_fnv(mut digest: u64) -> String {
    digest ^= 0xff;
    digest = digest.wrapping_mul(FNV_PRIME);
    format!("fnv1a64:{digest:016x}")
}

pub fn harness_git_revision() -> &'static str {
    static REVISION: OnceLock<String> = OnceLock::new();
    REVISION.get_or_init(|| {
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|revision| revision.trim().to_owned())
            .filter(|revision| !revision.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    })
}

pub fn harness_git_dirty_state() -> &'static str {
    static DIRTY_STATE: OnceLock<String> = OnceLock::new();
    DIRTY_STATE.get_or_init(|| {
        Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=normal"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                if output.stdout.is_empty() {
                    "clean".to_owned()
                } else {
                    "dirty".to_owned()
                }
            })
            .unwrap_or_else(|| "unknown".to_owned())
    })
}
