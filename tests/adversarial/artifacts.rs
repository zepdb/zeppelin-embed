use std::fs;
use std::path::{Path, PathBuf};

use super::campaign::{CampaignKind, FeatureFaultEvent};
use super::coverage::CoverageRegistry;
use super::fault_vfs::FaultEvent;
use super::oracle::OracleRecord;
use super::profiles::FaultProfile;
use super::program::Program;
use super::runner::Violation;

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
    ) -> Result<Vec<u8>, String> {
        let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "zeppelin-embed-adversarial-episode",
            "version": 3,
            "campaign": campaign.key(),
            "seed": seed,
            "profile": profile.key(),
            "reproduction": reproduction,
        }))
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

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}
