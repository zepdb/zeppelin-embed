use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::campaign::{CampaignKind, FeatureFaultEvent};
use super::coverage::CoverageRegistry;
use super::fault_vfs::FaultEvent;
use super::oracle::OracleRecord;
use super::profiles::FaultProfile;
use super::program::Program;
use super::runner::Violation;

pub const REPLAY_ARTIFACTS: [&str; 8] = [
    "program.jsonl",
    "faults.jsonl",
    "violations.json",
    "coverage.json",
    "oracle.jsonl",
    "controls.jsonl",
    "receipts.jsonl",
    "mutations.jsonl",
];
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpisodeAttestation {
    pub comparison_counts: BTreeMap<String, u64>,
    pub same_seed_clean_controls: u64,
    pub integrated_feature_fault_receipts: u64,
    pub evidence_digests: BTreeMap<String, String>,
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
    oracle: MergedStream,
    controls: MergedStream,
    receipts: MergedStream,
    mutations: MergedStream,
    index: File,
    episodes: u64,
}

struct MergedStream {
    name: &'static str,
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
            oracle: MergedStream::create(root, "oracle")?,
            controls: MergedStream::create(root, "controls")?,
            receipts: MergedStream::create(root, "receipts")?,
            mutations: MergedStream::create(root, "mutations")?,
            index: create_merged_file(root, "merged-index.jsonl")?,
            episodes: 0,
        };
        evidence.sync_all()?;
        Ok(evidence)
    }

    pub fn append_episode(
        &mut self,
        seed: u64,
        profile: FaultProfile,
        oracle: &[u8],
        controls: &[u8],
        receipts: &[u8],
        mutations: &[u8],
    ) -> Result<(), String> {
        let oracle_episode = self.oracle.append(seed, profile, oracle)?;
        let controls_episode = self.controls.append(seed, profile, controls)?;
        let receipts_episode = self.receipts.append(seed, profile, receipts)?;
        let mutations_episode = self.mutations.append(seed, profile, mutations)?;
        self.oracle.sync()?;
        self.controls.sync()?;
        self.receipts.sync()?;
        self.mutations.sync()?;

        let mut index = zeppelin_embed_bench::harness_json::to_vec(
            &zeppelin_embed_bench::harness_json::json!({
                "campaign_episode": self.episodes,
                "seed": seed,
                "profile": profile.key(),
                "streams": {
                    "oracle": oracle_episode,
                    "controls": controls_episode,
                    "receipts": receipts_episode,
                    "mutations": mutations_episode,
                },
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
        File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("sync merged evidence directory: {error}"))?;
        self.episodes = self.episodes.saturating_add(1);
        Ok(())
    }

    #[must_use]
    pub fn stats(&self) -> MergedEvidenceStats {
        MergedEvidenceStats {
            episodes: self.episodes,
            streams: [
                self.oracle.stats(),
                self.controls.stats(),
                self.receipts.stats(),
                self.mutations.stats(),
            ]
            .into_iter()
            .map(|(name, stats)| (name.to_owned(), stats))
            .collect(),
        }
    }

    fn sync_all(&mut self) -> Result<(), String> {
        self.oracle.sync()?;
        self.controls.sync()?;
        self.receipts.sync()?;
        self.mutations.sync()?;
        self.index
            .sync_all()
            .map_err(|error| format!("sync merged-index.jsonl: {error}"))?;
        File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("sync merged evidence directory: {error}"))
    }
}

impl MergedStream {
    fn create(root: &Path, name: &'static str) -> Result<Self, String> {
        Ok(Self {
            name,
            file: create_merged_file(root, &format!("merged-{name}.jsonl"))?,
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
        }))
    }

    fn sync(&self) -> Result<(), String> {
        self.file
            .sync_all()
            .map_err(|error| format!("sync merged-{}.jsonl: {error}", self.name))
    }

    fn stats(&self) -> (&'static str, MergedStreamStats) {
        (
            self.name,
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
        let mut bytes = Vec::new();
        for fault in faults {
            bytes.extend_from_slice(fault.json_line().as_bytes());
            bytes.push(b'\n');
        }
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
        let mut bytes = Vec::new();
        for fault in faults {
            bytes.extend_from_slice(fault.json_line().as_bytes());
            bytes.push(b'\n');
        }
        for fault in feature_faults {
            bytes.extend_from_slice(fault.json_line().as_bytes());
            bytes.push(b'\n');
        }
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
        reproduction: &str,
        attestation: Option<&EpisodeAttestation>,
    ) -> Result<Vec<u8>, String> {
        let metadata = if let Some(attestation) = attestation {
            zeppelin_embed_bench::harness_json::json!({
                "schema": "zeppelin-embed-adversarial-episode",
                "version": 3,
                "campaign": campaign.key(),
                "seed": seed,
                "profile": profile.key(),
                "reproduction": reproduction,
                "attestation": {
                    "oracle_contract_version": zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION,
                    "harness_git_revision": harness_git_revision(),
                    "comparison_counts": attestation.comparison_counts,
                    "same_seed_clean_controls": attestation.same_seed_clean_controls,
                    "integrated_feature_fault_receipts": attestation.integrated_feature_fault_receipts,
                    "evidence_digests": attestation.evidence_digests,
                    "replay_artifacts": REPLAY_ARTIFACTS,
                },
            })
        } else {
            zeppelin_embed_bench::harness_json::json!({
                "schema": "zeppelin-embed-adversarial-episode",
                "version": 3,
                "campaign": campaign.key(),
                "seed": seed,
                "profile": profile.key(),
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

fn harness_git_revision() -> &'static str {
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
