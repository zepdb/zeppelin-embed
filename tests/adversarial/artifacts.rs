use std::collections::BTreeMap;
use std::fs;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpisodeAttestation {
    pub comparison_counts: BTreeMap<String, u64>,
    pub same_seed_clean_controls: u64,
    pub integrated_feature_fault_receipts: u64,
    pub evidence_digests: BTreeMap<String, String>,
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
    let mut digest = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        for byte in *part {
            digest ^= u64::from(*byte);
            digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
        }
        digest ^= 0xff;
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
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
