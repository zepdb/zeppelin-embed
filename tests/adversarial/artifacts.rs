use std::fs;
use std::path::{Path, PathBuf};

use super::fault_vfs::FaultEvent;
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

    pub fn write_violations(&self, violations: &[Violation]) -> Result<Vec<u8>, String> {
        let mut text = String::from("[\n");
        for (index, violation) in violations.iter().enumerate() {
            if index != 0 {
                text.push_str(",\n");
            }
            text.push_str("  ");
            text.push_str(&violation.json());
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

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}
