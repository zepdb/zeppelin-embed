#![allow(clippy::expect_used)]

mod lifecycle_support;

use std::sync::mpsc;
use std::time::Duration;

use lifecycle_support::{published_store, test_guard};
use tempfile::tempdir;
#[cfg(unix)]
use zeppelin_embed::lifecycle::lock::{STORE_LOCK_FILE, StoreLock};
use zeppelin_embed::lifecycle::{OpenOptions, Store, StoreError};

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
