#![allow(clippy::expect_used)]

mod lifecycle_support;

use std::sync::mpsc;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(unix)]
use std::process::{Command, Stdio};

use lifecycle_support::{published_store, test_guard};
use tempfile::tempdir;
#[cfg(unix)]
use zeppelin_embed::lifecycle::lock::{STORE_LOCK_FILE, StoreLock};
use zeppelin_embed::lifecycle::{OpenOptions, Store, StoreError};

#[cfg(target_os = "macos")]
fn descriptor_path(raw_fd: i32) -> Option<std::path::PathBuf> {
    unsafe extern "C" {
        fn fcntl(fd: i32, command: i32, ...) -> i32;
    }
    const F_GETPATH: i32 = 50;
    const MAX_PATH_BYTES: usize = 1_024;
    let mut path = [0_i8; MAX_PATH_BYTES];
    let status = unsafe {
        // SAFETY: F_GETPATH writes a NUL-terminated path into this fixed-size
        // buffer and does not retain the pointer.
        fcntl(raw_fd, F_GETPATH, path.as_mut_ptr())
    };
    if status == -1 {
        return None;
    }
    let path = unsafe {
        // SAFETY: successful F_GETPATH initialized a NUL-terminated string.
        std::ffi::CStr::from_ptr(path.as_ptr())
    };
    use std::os::unix::ffi::OsStrExt as _;
    Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
        path.to_bytes(),
    )))
}

#[cfg(target_os = "linux")]
fn descriptor_path(raw_fd: i32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{raw_fd}")).ok()
}

#[cfg(unix)]
fn store_lock_descriptor(directory: &std::path::Path) -> i32 {
    let lock_path =
        std::fs::canonicalize(directory.join(STORE_LOCK_FILE)).expect("resolve writer lock path");
    (3..256)
        .find(|raw_fd| descriptor_path(*raw_fd).as_deref() == Some(lock_path.as_path()))
        .expect("open writer-lock descriptor")
}

#[test]
#[cfg(unix)]
fn reopen_succeeds_on_first_attempt_while_a_spawned_child_holds_an_inherited_lock_descriptor() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let mut store = Store::open(directory.path(), OpenOptions::default()).expect("initial writer");
    let lock_fd = store_lock_descriptor(directory.path());
    let mut command = Command::new("/bin/cat");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        // SAFETY: this closure performs only the async-signal-safe `dup2`
        // between fork and exec. The Store keeps `lock_fd` live through spawn.
        command.pre_exec(move || {
            if libc::dup2(lock_fd, 198) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().expect("spawn lock descriptor child");

    for cycle in 0..8 {
        store.close().expect("close writer while child is live");
        store = Store::open(directory.path(), OpenOptions::default()).unwrap_or_else(|error| {
            panic!("reopen cycle {cycle} failed on its first attempt: {error}")
        });
    }

    drop(child.stdin.take());
    assert!(
        child
            .wait()
            .expect("wait for lock descriptor child")
            .success()
    );
    store.close().expect("close final writer");
}

#[test]
#[ignore = "subprocess-only helper selected by the parent lock probe"]
#[cfg(unix)]
fn lock_probe_child() {
    let directory = std::env::var_os("ZE_LOCK_PROBE_DIR")
        .map(std::path::PathBuf::from)
        .expect("lock probe directory");
    let exit_code = match StoreLock::acquire(&directory) {
        Err(zeppelin_embed::lifecycle::lock::StoreLockError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::WouldBlock =>
        {
            0
        }
        Ok(lock) => {
            drop(lock);
            2
        }
        Err(error) => panic!("lock probe returned an unexpected error: {error}"),
    };
    std::process::exit(exit_code);
}

#[test]
#[cfg(unix)]
fn a_refused_in_process_second_acquire_does_not_release_the_kernel_lock() {
    fn probe(directory: &std::path::Path) -> std::process::ExitStatus {
        Command::new(std::env::current_exe().expect("single-writer test executable"))
            .args(["--exact", "lock_probe_child", "--ignored"])
            .env("ZE_LOCK_PROBE_DIR", directory)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn lock probe child")
    }

    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let first = StoreLock::acquire(directory.path()).expect("first writer lock");
    let second = StoreLock::acquire(directory.path()).expect_err("second writer lock refused");
    let second_kind = match second {
        zeppelin_embed::lifecycle::lock::StoreLockError::Io { source, .. } => source.kind(),
    };
    assert_eq!(second_kind, std::io::ErrorKind::WouldBlock);

    assert_eq!(
        probe(directory.path()).code(),
        Some(0),
        "refused same-process acquire released the kernel lock"
    );
    drop(first);
    assert_eq!(
        probe(directory.path()).code(),
        Some(2),
        "dropping the owner did not release the kernel lock"
    );
}

#[test]
fn second_write_open_returns_typed_store_busy_immediately() {
    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    let first = Store::open(directory.path(), OpenOptions::default()).expect("first writer");
    let second_path = directory.path().to_path_buf();
    let (returned_tx, returned_rx) = mpsc::sync_channel(0);
    let second = std::thread::spawn(move || {
        let result = Store::open(second_path, OpenOptions::default());
        returned_tx.send(result).expect("report second open");
    });

    let result = match returned_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(result) => result,
        Err(error) => {
            first.close().expect("release writer after blocked open");
            let _ = returned_rx.recv_timeout(Duration::from_secs(5));
            second.join().expect("blocked writer thread");
            panic!("second writer did not fail immediately: {error}");
        }
    };
    assert!(matches!(result, Err(StoreError::StoreBusy { .. })));
    second.join().expect("second writer thread");
    first.close().expect("close first writer");
}

#[test]
fn read_only_open_with_live_writer_serves_last_published_snapshot() {
    let _guard = test_guard();
    let fixture = published_store(73);
    let writer = Store::open(fixture.path(), OpenOptions::default()).expect("writer");

    let reader = Store::open(fixture.path(), OpenOptions::read_only()).expect("read-only open");
    let snapshot = reader.snapshot().expect("read-only snapshot");

    assert_eq!(snapshot.generation(), 73);
    assert_eq!(snapshot.segments().len(), 1);
    assert_eq!(
        snapshot
            .segments()
            .first()
            .expect("published segment")
            .bit4_codes()
            .expect("mapped codes"),
        &[0x88]
    );
    drop(snapshot);
    reader.close().expect("close reader");
    writer.close().expect("close writer");
}

#[test]
#[cfg(unix)]
fn pure_read_open_creates_no_files_and_takes_no_write_locks() {
    use std::os::unix::fs::PermissionsExt;

    struct PermissionRestore {
        path: std::path::PathBuf,
        permissions: std::fs::Permissions,
    }

    impl Drop for PermissionRestore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.path, self.permissions.clone());
        }
    }

    fn entries(path: &std::path::Path) -> Vec<std::ffi::OsString> {
        let mut entries = std::fs::read_dir(path)
            .expect("read store directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    let _guard = test_guard();
    let directory = tempdir().expect("store directory");
    std::fs::File::create(directory.path().join(STORE_LOCK_FILE)).expect("existing lock file");
    let original_permissions = std::fs::metadata(directory.path())
        .expect("directory metadata")
        .permissions();
    let _restore = PermissionRestore {
        path: directory.path().to_path_buf(),
        permissions: original_permissions,
    };
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o555))
        .expect("make store directory read-only");
    let before = entries(directory.path());

    let reader = Store::open(directory.path(), OpenOptions::read_only())
        .expect("pure read open on read-only directory");
    let lock_probe = StoreLock::acquire(directory.path()).expect("reader took no writer lock");

    assert_eq!(
        entries(directory.path()),
        before,
        "pure read created a file"
    );
    assert_eq!(reader.snapshot().expect("empty snapshot").generation(), 0);
    drop(lock_probe);
    reader.close().expect("close reader");
}

#[test]
fn open_filesystem_rejections_are_typed() {
    fn open_error(result: Result<Store, StoreError>) -> StoreError {
        match result {
            Ok(store) => {
                drop(store);
                panic!("invalid store path unexpectedly opened")
            }
            Err(error) => error,
        }
    }

    let _guard = test_guard();
    let parent = tempdir().expect("parent directory");
    let file_path = parent.path().join("not-a-directory");
    std::fs::File::create(&file_path).expect("plain file");

    let not_directory = open_error(Store::open(&file_path, OpenOptions::read_only()));
    assert!(matches!(not_directory, StoreError::NotDirectory { .. }));
    assert!(not_directory.to_string().contains("not a directory"));

    let missing = parent.path().join("missing");
    let missing_error = open_error(Store::open(&missing, OpenOptions::read_only()));
    match missing_error {
        StoreError::Io { path, source } => {
            assert_eq!(path, missing);
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("missing read-only path returned {other}"),
    }

    let child_of_file = file_path.join("child");
    let create_error = open_error(Store::open(&child_of_file, OpenOptions::default()));
    assert!(matches!(create_error, StoreError::Io { .. }));
}
