#![allow(clippy::expect_used, clippy::panic)]

use super::{LockMode, StoreLock, StoreLockError, held_stores, store_identity};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{OpenOptions, Store};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CHILD_HELPER: &str = "lifecycle::lock::native_tests::native_lock_child_helper";
const CHILD_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildMode {
    Shared,
    Exclusive,
}

impl ChildMode {
    const fn name(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Exclusive => "exclusive",
        }
    }
}

struct ChildProbe {
    child: Child,
    stdin: Option<ChildStdin>,
    _stdout: BufReader<ChildStdout>,
    pid: u32,
    mode: ChildMode,
    outcome: String,
}

impl ChildProbe {
    fn assert_holds(&self) {
        assert_eq!(
            self.outcome,
            format!("ZE_NATIVE_LOCK ACK {} {}", self.pid, self.mode.name()),
            "child {} did not acknowledge its {} lock: {}",
            self.pid,
            self.mode.name(),
            self.outcome
        );
    }

    fn assert_conflict(&mut self) {
        assert_eq!(
            self.outcome,
            format!("ZE_NATIVE_LOCK CONFLICT {} {}", self.pid, self.mode.name()),
            "child {} did not report a {} conflict: {}",
            self.pid,
            self.mode.name(),
            self.outcome
        );
        assert!(wait_bounded(&mut self.child).success());
    }

    fn release(mut self) -> u32 {
        let mut stdin = self.stdin.take().expect("held child stdin");
        stdin.write_all(b"release\n").expect("release child lock");
        stdin.flush().expect("flush child release");
        drop(stdin);
        assert!(wait_bounded(&mut self.child).success());
        self.pid
    }

    fn kill(mut self) -> u32 {
        self.child.kill().expect("kill owned lock child");
        assert!(!wait_bounded(&mut self.child).success());
        self.pid
    }
}

impl Drop for ChildProbe {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn wait_bounded(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => return status,
            None if Instant::now() < deadline => std::thread::yield_now(),
            None => {
                child.kill().expect("kill timed-out child");
                return child.wait().expect("reap timed-out child");
            }
        }
    }
}

fn spawn_probe(directory: &Path, mode: ChildMode) -> ChildProbe {
    let mut child = Command::new(std::env::current_exe().expect("unit-test executable"))
        .args(["--exact", CHILD_HELPER, "--nocapture"])
        .env("ZE_NATIVE_LOCK_CHILD_DIR", directory)
        .env("ZE_NATIVE_LOCK_CHILD_MODE", mode.name())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn native-lock child");
    let pid = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("child stdout");
    let (line_tx, line_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = line_tx.send(("ZE_NATIVE_LOCK EOF".to_owned(), reader));
                    break;
                }
                Ok(_) if line.starts_with("ZE_NATIVE_LOCK ") => {
                    let _ = line_tx.send((line.trim_end().to_owned(), reader));
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = line_tx.send((format!("ZE_NATIVE_LOCK READ_ERROR {error}"), reader));
                    break;
                }
            }
        }
    });
    let (outcome, stdout) = match line_rx.recv_timeout(CHILD_TIMEOUT) {
        Ok(result) => result,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child {pid} acknowledgement timed out: {error}");
        }
    };
    ChildProbe {
        child,
        stdin,
        _stdout: stdout,
        pid,
        mode,
        outcome,
    }
}

fn fixture_with_live_writer() -> (tempfile::TempDir, PathBuf, Store) {
    let parent = tempfile::tempdir().expect("native fixture parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(
        &path,
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native fixture");
    (parent, path, store)
}

fn directory_entries(directory: &Path) -> Vec<std::ffi::OsString> {
    let mut entries = std::fs::read_dir(directory)
        .expect("read directory inventory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn error_kind(error: &StoreLockError) -> std::io::ErrorKind {
    match error {
        StoreLockError::Io { source, .. } => source.kind(),
    }
}

#[test]
fn native_lock_child_helper() {
    let Some(directory) = std::env::var_os("ZE_NATIVE_LOCK_CHILD_DIR").map(PathBuf::from) else {
        return;
    };
    let mode = std::env::var("ZE_NATIVE_LOCK_CHILD_MODE").expect("native-lock child mode");
    let acquired = match mode.as_str() {
        "shared" => StoreLock::acquire_shared(&directory),
        "exclusive" => StoreLock::acquire(&directory),
        other => panic!("unknown child lock mode {other}"),
    };
    match acquired {
        Ok(lock) => {
            println!("ZE_NATIVE_LOCK ACK {} {mode}", std::process::id());
            std::io::stdout()
                .flush()
                .expect("flush child acknowledgement");
            let mut release = [0_u8; 1];
            let _ = std::io::stdin().read(&mut release);
            drop(lock);
        }
        Err(error) if error_kind(&error) == std::io::ErrorKind::WouldBlock => {
            println!("ZE_NATIVE_LOCK CONFLICT {} {mode}", std::process::id());
            std::io::stdout().flush().expect("flush child conflict");
        }
        Err(error) => {
            println!(
                "ZE_NATIVE_LOCK ERROR {} {mode} {:?}",
                std::process::id(),
                error_kind(&error)
            );
            std::io::stdout().flush().expect("flush child error");
        }
    }
}

#[test]
fn native_shared_lock_allows_two_processes_and_excludes_writer() {
    let (_parent, directory, store) = fixture_with_live_writer();

    let mut blocked_reader = spawn_probe(&directory, ChildMode::Shared);
    blocked_reader.assert_conflict();
    store.close().expect("close native fixture writer");

    let first_reader = spawn_probe(&directory, ChildMode::Shared);
    first_reader.assert_holds();
    let second_reader = spawn_probe(&directory, ChildMode::Shared);
    second_reader.assert_holds();

    let mut writer = spawn_probe(&directory, ChildMode::Exclusive);
    writer.assert_conflict();
    let first_pid = first_reader.release();
    let second_pid = second_reader.release();
    eprintln!("acknowledged native shared lock child pids: first={first_pid} second={second_pid}");
}

#[test]
fn native_shared_lock_retains_kernel_ownership_until_last_local_drop() {
    let (_parent, directory, store) = fixture_with_live_writer();
    store.close().expect("close native fixture writer");
    let alias = directory.join(".");

    let first = StoreLock::acquire_shared(&directory).expect("first local reader");
    let key = store_identity(&directory).expect("native fixture identity");
    {
        let held = held_stores();
        let entry = held.get(&key).expect("first registry claim");
        assert_eq!(entry.mode, LockMode::Shared);
        assert_eq!(entry.shared_count, 1);
    }
    let second = StoreLock::acquire_shared(&alias).expect("aliased second local reader");
    {
        let held = held_stores();
        let entry = held.get(&key).expect("shared registry claim");
        assert_eq!(entry.shared_count, 2);
    }

    assert_eq!(
        error_kind(&StoreLock::acquire(&directory).expect_err("local writer conflict")),
        std::io::ErrorKind::WouldBlock
    );
    let mut writer = spawn_probe(&directory, ChildMode::Exclusive);
    writer.assert_conflict();

    drop(first);
    let mut writer = spawn_probe(&directory, ChildMode::Exclusive);
    writer.assert_conflict();
    {
        let held = held_stores();
        assert_eq!(
            held.get(&key)
                .expect("registry retained after partial drop")
                .shared_count,
            1
        );
    }

    drop(second);
    assert!(held_stores().get(&key).is_none());
    let writer = spawn_probe(&directory, ChildMode::Exclusive);
    writer.assert_holds();
    let writer_pid = writer.release();
    eprintln!("acknowledged native exclusive lock child pid after final drop: {writer_pid}");
}

#[test]
fn native_shared_lock_rejects_conflicting_modes_and_preserves_errors() {
    let (_parent, directory, store) = fixture_with_live_writer();
    assert_eq!(
        error_kind(
            &StoreLock::acquire_shared(&directory)
                .expect_err("local shared versus writer conflict")
        ),
        std::io::ErrorKind::WouldBlock
    );
    store.close().expect("close native fixture writer");

    let reader = StoreLock::acquire_shared(&directory).expect("local shared owner");
    assert_eq!(
        error_kind(
            &StoreLock::acquire(&directory).expect_err("local writer versus shared conflict")
        ),
        std::io::ErrorKind::WouldBlock
    );
    let key = store_identity(&directory).expect("native fixture identity");
    {
        let mut held = held_stores();
        held.get_mut(&key)
            .expect("shared registry entry")
            .shared_count = u32::MAX;
    }
    let overflow = StoreLock::acquire_shared(&directory).expect_err("shared count overflow");
    assert_eq!(error_kind(&overflow), std::io::ErrorKind::Other);
    assert!(overflow.to_string().contains("holder count overflow"));
    {
        let mut held = held_stores();
        let entry = held
            .get_mut(&key)
            .expect("overflow retained registry entry");
        assert_eq!(entry.shared_count, u32::MAX);
        entry.shared_count = 1;
    }
    let mut writer = spawn_probe(&directory, ChildMode::Exclusive);
    writer.assert_conflict();
    drop(reader);

    let writer = spawn_probe(&directory, ChildMode::Exclusive);
    writer.assert_holds();
    assert_eq!(
        error_kind(
            &StoreLock::acquire_shared(&directory)
                .expect_err("parent shared versus child writer conflict")
        ),
        std::io::ErrorKind::WouldBlock
    );
    let writer_pid = writer.release();

    let missing_parent = tempfile::tempdir().expect("missing-lock parent");
    let missing_store = missing_parent.path().join("store");
    std::fs::create_dir(&missing_store).expect("missing-lock store directory");
    let before = directory_entries(&missing_store);
    let missing = StoreLock::acquire_shared(&missing_store).expect_err("missing lock file");
    assert_eq!(error_kind(&missing), std::io::ErrorKind::NotFound);
    assert_eq!(directory_entries(&missing_store), before);

    let invalid_parent = tempfile::tempdir().expect("invalid-lock parent");
    let invalid_store = invalid_parent.path().join("store");
    std::fs::create_dir(&invalid_store).expect("invalid-lock store directory");
    let invalid_lock = invalid_store.join(super::STORE_LOCK_FILE);
    std::fs::create_dir(&invalid_lock).expect("directory at lock path");
    let directory_error =
        StoreLock::acquire_shared(&invalid_store).expect_err("directory lock path rejected");
    assert_ne!(error_kind(&directory_error), std::io::ErrorKind::WouldBlock);
    std::fs::remove_dir(&invalid_lock).expect("remove directory lock path");
    std::fs::write(&invalid_lock, b"valid-lock-bytes").expect("replace valid lock file");
    drop(StoreLock::acquire_shared(&invalid_store).expect("valid acquisition after IO failure"));

    eprintln!(
        "acknowledged native exclusive lock child pid={writer_pid}; missing={:?}; directory={:?}; overflow={:?}",
        error_kind(&missing),
        error_kind(&directory_error),
        error_kind(&overflow)
    );
}

#[test]
fn native_shared_lock_releases_on_child_exit_and_kill() {
    let (_parent, directory, store) = fixture_with_live_writer();
    store.close().expect("close native fixture writer");

    let reader = spawn_probe(&directory, ChildMode::Shared);
    reader.assert_holds();
    assert_eq!(
        error_kind(&StoreLock::acquire(&directory).expect_err("reader excludes writer")),
        std::io::ErrorKind::WouldBlock
    );
    let orderly_pid = reader.release();
    drop(StoreLock::acquire(&directory).expect("first writer attempt after orderly exit"));

    let writer = spawn_probe(&directory, ChildMode::Exclusive);
    writer.assert_holds();
    assert_eq!(
        error_kind(
            &StoreLock::acquire_shared(&directory).expect_err("writer excludes shared reader")
        ),
        std::io::ErrorKind::WouldBlock
    );
    let killed_pid = writer.kill();
    drop(
        StoreLock::acquire_shared(&directory)
            .expect("first shared attempt after killed child exit"),
    );

    eprintln!(
        "acknowledged native lock child pids: orderly-shared={orderly_pid} killed-exclusive={killed_pid}"
    );
}

#[test]
fn native_shared_lock_never_creates_or_writes_lock_file() {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::PermissionsExt as _;

    struct PermissionRestore {
        path: PathBuf,
        permissions: std::fs::Permissions,
    }

    impl Drop for PermissionRestore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.path, self.permissions.clone());
        }
    }

    let (_parent, directory, store) = fixture_with_live_writer();
    store.close().expect("close native fixture writer");
    let lock_path = directory.join(super::STORE_LOCK_FILE);
    let original_permissions = std::fs::metadata(&lock_path)
        .expect("lock metadata")
        .permissions();
    let _restore = PermissionRestore {
        path: lock_path.clone(),
        permissions: original_permissions.clone(),
    };
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o444))
        .expect("make lock file read-only");
    let before_bytes = std::fs::read(&lock_path).expect("initial lock bytes");
    let before_length = std::fs::metadata(&lock_path)
        .expect("initial lock metadata")
        .len();
    let before_entries = directory_entries(&directory);

    let reader = StoreLock::acquire_shared(&directory).expect("read-only lock-file acquisition");
    let key = store_identity(&directory).expect("native fixture identity");
    let flags = {
        let held = held_stores();
        let entry = held.get(&key).expect("shared registry entry");
        unsafe {
            // SAFETY: the registry owns this live descriptor and F_GETFL does
            // not mutate it or retain any pointer.
            libc::fcntl(entry._file.as_raw_fd(), libc::F_GETFL)
        }
    };
    assert_ne!(flags, -1, "inspect shared lock descriptor flags");
    assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
    assert_eq!(
        std::fs::read(&lock_path).expect("held lock bytes"),
        before_bytes
    );
    assert_eq!(
        std::fs::metadata(&lock_path)
            .expect("held lock metadata")
            .len(),
        before_length
    );
    assert_eq!(directory_entries(&directory), before_entries);
    drop(reader);

    std::fs::set_permissions(&lock_path, original_permissions.clone())
        .expect("restore writer lock permissions");
    let writer = spawn_probe(&directory, ChildMode::Exclusive);
    writer.assert_holds();
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o444))
        .expect("make held writer lock file read-only");
    let failed =
        StoreLock::acquire_shared(&directory).expect_err("shared conflict with child writer");
    assert_eq!(error_kind(&failed), std::io::ErrorKind::WouldBlock);
    assert_eq!(
        std::fs::read(&lock_path).expect("failed lock bytes"),
        before_bytes
    );
    assert_eq!(
        std::fs::metadata(&lock_path)
            .expect("failed lock metadata")
            .len(),
        before_length
    );
    assert_eq!(directory_entries(&directory), before_entries);
    let writer_pid = writer.release();

    assert_eq!(
        std::fs::read(&lock_path).expect("final lock bytes"),
        before_bytes
    );
    assert_eq!(directory_entries(&directory), before_entries);
    eprintln!(
        "acknowledged native exclusive lock child pid={writer_pid}; shared descriptor flags={flags:#x}; bytes={before_length}"
    );
}
