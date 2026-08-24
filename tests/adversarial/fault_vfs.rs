use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rand::Rng;
use zeppelin_embed::vfs::crash::{CrashVfs, MemoryVfs};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

use super::profiles::FaultProfile;
use super::test_support;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultMode {
    Eio,
    Eacces,
    Enospc,
    BitFlip,
    TornWrite,
    Truncate,
    ZeroFill,
}

impl FaultMode {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Eio => "eio",
            Self::Eacces => "eacces",
            Self::Enospc => "enospc",
            Self::BitFlip => "bit_flip",
            Self::TornWrite => "torn_write",
            Self::Truncate => "truncate",
            Self::ZeroFill => "zero_fill",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultSite {
    Write,
}

impl FaultSite {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaultEvent {
    pub id: String,
    pub op_index: usize,
    pub site: FaultSite,
    pub mode: FaultMode,
    pub nth_match: usize,
    pub fired: bool,
    pub path: Option<PathBuf>,
}

impl FaultEvent {
    #[must_use]
    pub fn json_line(&self) -> String {
        let path = self.path.as_ref().map_or_else(
            || "null".to_owned(),
            |path| format!("\"{}\"", json_escape(&path.display().to_string())),
        );
        format!(
            "{{\"id\":\"{}\",\"op\":{},\"site\":\"{}\",\"mode\":\"{}\",\"nth_match\":{},\"fired\":{},\"path\":{path}}}",
            self.id,
            self.op_index,
            self.site.key(),
            self.mode.key(),
            self.nth_match,
            self.fired
        )
    }
}

#[derive(Clone, Debug, Default)]
struct Runtime {
    matches: usize,
    fired: bool,
    path: Option<PathBuf>,
}

#[derive(Clone)]
pub struct ScheduledVfs<V> {
    inner: V,
    event: Option<FaultEvent>,
    runtime: Arc<Mutex<Runtime>>,
}

impl<V> ScheduledVfs<V> {
    #[must_use]
    pub fn new(inner: V, event: Option<FaultEvent>) -> Self {
        Self {
            inner,
            event,
            runtime: Arc::new(Mutex::new(Runtime::default())),
        }
    }

    #[must_use]
    pub fn event(&self) -> Option<FaultEvent> {
        let mut event = self.event.clone()?;
        let runtime = self.runtime.lock().expect("fault runtime mutex");
        event.fired = runtime.fired;
        event.path.clone_from(&runtime.path);
        Some(event)
    }

    fn write_action(&self, path: &Path, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        let Some(event) = &self.event else {
            return Ok(bytes.to_vec());
        };
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| std::io::Error::other("scheduled fault runtime mutex poisoned"))?;
        runtime.matches = runtime.matches.saturating_add(1);
        if runtime.fired || runtime.matches != event.nth_match {
            return Ok(bytes.to_vec());
        }
        runtime.fired = true;
        runtime.path = Some(path.to_path_buf());
        match event.mode {
            FaultMode::Eio => Err(std::io::Error::from_raw_os_error(5)),
            FaultMode::Eacces => Err(std::io::Error::from_raw_os_error(13)),
            FaultMode::Enospc => Err(std::io::Error::from_raw_os_error(28)),
            FaultMode::BitFlip => {
                let mut damaged = bytes.to_vec();
                let offset = damaged.len().saturating_sub(1) / 2;
                if let Some(byte) = damaged.get_mut(offset) {
                    *byte ^= 0x01;
                }
                Ok(damaged)
            }
            FaultMode::TornWrite => Ok(bytes.get(..bytes.len() / 2).unwrap_or_default().to_vec()),
            FaultMode::Truncate => Ok(bytes.get(..bytes.len() / 3).unwrap_or_default().to_vec()),
            FaultMode::ZeroFill => Ok(vec![0_u8; bytes.len()]),
        }
    }
}

struct ScheduledFile {
    inner: Box<dyn VfsFile>,
}

impl VfsFile for ScheduledFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        self.inner.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(kind)
    }
}

impl<V: Vfs> Vfs for ScheduledVfs<V> {
    fn segment_data_read_counter(&self) -> Option<Arc<std::sync::atomic::AtomicU64>> {
        self.inner.segment_data_read_counter()
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let bytes = self.write_action(path, bytes)?;
        self.inner.write(path, &bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        Ok(Box::new(ScheduledFile {
            inner: self.inner.open_append(path)?,
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)
    }
}

#[must_use]
pub fn scheduled_event(seed: u64, profile: FaultProfile, op_index: usize) -> Option<FaultEvent> {
    let mode = match profile {
        FaultProfile::None | FaultProfile::Crash | FaultProfile::Clock => return None,
        FaultProfile::IoErrors => {
            let mut rng = test_support::seeded_rng("adversarial::io-fault", seed);
            if rng.random::<bool>() {
                FaultMode::Eio
            } else {
                FaultMode::Eacces
            }
        }
        FaultProfile::Content => {
            let modes = [
                FaultMode::BitFlip,
                FaultMode::TornWrite,
                FaultMode::Truncate,
                FaultMode::ZeroFill,
            ];
            let mut rng = test_support::seeded_rng("adversarial::content-fault", seed);
            modes[rng.random_range(0..modes.len())]
        }
        FaultProfile::Disk => FaultMode::Enospc,
        FaultProfile::Full => FaultMode::BitFlip,
    };
    Some(FaultEvent {
        id: format!("{}-{seed}-{op_index}", profile.key()),
        op_index,
        site: FaultSite::Write,
        mode,
        nth_match: 1,
        fired: false,
        path: None,
    })
}

/// Exercises the repository's crash-state materializer for every crash schedule.
pub fn audit_crash_seam(seed: u64, op_index: usize) -> Result<FaultEvent, String> {
    let backing = MemoryVfs::new();
    let crash = CrashVfs::new(backing).map_err(|error| error.to_string())?;
    let first = PathBuf::from(format!("/adv-{seed}-{op_index}.tmp"));
    let final_path = PathBuf::from(format!("/adv-{seed}-{op_index}.bin"));
    crash
        .write(&first, b"adversarial-crash-boundary")
        .map_err(|error| error.to_string())?;
    crash
        .sync(&first, SyncKind::Full)
        .map_err(|error| error.to_string())?;
    crash
        .rename(&first, &final_path)
        .map_err(|error| error.to_string())?;
    let states = crash.crash_states().map_err(|error| error.to_string())?;
    if states.is_empty() {
        return Err("vfs::crash produced no crash states".to_owned());
    }
    Ok(FaultEvent {
        id: format!("crash-{seed}-{op_index}"),
        op_index,
        site: FaultSite::Write,
        mode: FaultMode::TornWrite,
        nth_match: states.len(),
        fired: true,
        path: Some(final_path),
    })
}

#[must_use]
pub fn std_scheduled(event: Option<FaultEvent>) -> ScheduledVfs<StdVfs> {
    ScheduledVfs::new(StdVfs, event)
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
