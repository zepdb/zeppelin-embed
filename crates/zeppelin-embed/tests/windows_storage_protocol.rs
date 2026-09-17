//! The W02 publication protocol, re-proved through the production `StdVfs`.
//!
//! `tools/windows-storage-probe` established the protocol against raw Win32
//! before the engine could compile on Windows. These tests assert the same
//! properties through the seam the engine actually uses, so a later change to
//! `StdVfs` or `sys::windows` that quietly breaks one of them fails here rather
//! than in production.
//!
//! Every test exercises real Win32 calls on a real NTFS directory; none uses an
//! injected or in-memory filesystem.

#![cfg(windows)]
#![allow(clippy::expect_used, clippy::panic)]

use std::io::Write as _;
use std::path::Path;

use tempfile::tempdir;
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs};

/// Raw Win32 error code behind an `io::Error`, or `None` if it carries none.
fn code_of(error: &std::io::Error) -> Option<u32> {
    error.raw_os_error().and_then(|raw| u32::try_from(raw).ok())
}

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_SHARING_VIOLATION: u32 = 32;

// ---------------------------------------------------------------------------
// Data durability through the production VFS.
// ---------------------------------------------------------------------------

/// `StdVfs::sync` previously reopened with `File::open`, which is
/// `GENERIC_READ`, and `FlushFileBuffers` refuses a read-only handle. Both
/// sync kinds must now succeed on an ordinary file.
#[test]
fn std_vfs_sync_flushes_a_file_for_both_sync_kinds() {
    let directory = tempdir().expect("scratch");
    let path = directory.path().join("artifact.bin");
    StdVfs.write(&path, b"durable-bytes").expect("write");

    StdVfs
        .sync(&path, SyncKind::Full)
        .expect("full sync of a file");
    StdVfs
        .sync(&path, SyncKind::Barrier)
        .expect("barrier sync of a file");

    assert_eq!(StdVfs.read(&path).expect("read"), b"durable-bytes");
}

/// The namespace flush: Windows has no `fsync(dirfd)` spelling, but a writable
/// directory handle opened with backup semantics accepts `FlushFileBuffers`.
/// This is what makes a published or retired *name* durable, and the engine's
/// manifest publication depends on it.
#[test]
fn std_vfs_sync_flushes_a_directory() {
    let directory = tempdir().expect("scratch");
    let nested = directory.path().join("namespace");
    std::fs::create_dir_all(&nested).expect("create namespace");
    StdVfs
        .write(&nested.join("manifest.zman"), b"generation-1")
        .expect("publish");

    StdVfs
        .sync(&nested, SyncKind::Full)
        .expect("full sync of a directory");
    StdVfs
        .sync(&nested, SyncKind::Barrier)
        .expect("barrier sync of a directory");
}

/// A sync of something that is not there must fail loudly with the platform's
/// own error, never succeed as a no-op.
#[test]
fn std_vfs_sync_of_a_missing_path_fails_loudly() {
    let directory = tempdir().expect("scratch");
    let missing_file = directory.path().join("absent.bin");
    let error = StdVfs
        .sync(&missing_file, SyncKind::Full)
        .expect_err("a missing file must not sync");
    assert_eq!(code_of(&error), Some(ERROR_FILE_NOT_FOUND));

    let missing_directory = directory.path().join("absent").join("deeper");
    let error = StdVfs
        .sync(&missing_directory, SyncKind::Full)
        .expect_err("a missing directory must not sync");
    assert_eq!(code_of(&error), Some(ERROR_PATH_NOT_FOUND));
}

// ---------------------------------------------------------------------------
// WAL appends through one retained handle.
// ---------------------------------------------------------------------------

/// The WAL opens its log with `OpenOptions::append`, which yields
/// `FILE_APPEND_DATA` rather than `GENERIC_WRITE`. An append-only access mask
/// is not automatically sufficient for `FlushFileBuffers`, and the handle must
/// never be reopened between appends, or a concurrent replacement of the path
/// could redirect the flush to a different file.
#[test]
fn wal_appends_and_flushes_reach_one_retained_file_object() {
    let directory = tempdir().expect("scratch");
    let path = directory.path().join("wal.ze");

    let mut handle = StdVfs.open_append(&path).expect("create the WAL");
    handle.append(b"record-one\n").expect("first append");
    handle.sync(SyncKind::Full).expect("flush an append handle");
    handle.append(b"record-two\n").expect("second append");
    handle
        .sync(SyncKind::Barrier)
        .expect("barrier flush an append handle");
    drop(handle);

    assert_eq!(
        StdVfs.read(&path).expect("read the WAL"),
        b"record-one\nrecord-two\n"
    );
}

// ---------------------------------------------------------------------------
// Publication and retirement under live readers.
// ---------------------------------------------------------------------------

/// The engine must be able to republish a manifest while readers hold it.
/// `MoveFileExW` cannot do this on Windows at any sharing mask, which is why
/// publication goes through the POSIX-semantics rename that `StdVfs::rename`
/// already issues. A reader that still holds the old name keeps reading.
#[test]
fn publication_replaces_a_name_a_reader_still_holds_open() {
    let directory = tempdir().expect("scratch");
    let manifest = directory.path().join("manifest.zman");
    let temporary = directory.path().join("manifest.zman.tmp");
    StdVfs.write(&manifest, b"generation-1").expect("seed");

    let reader = std::fs::File::open(&manifest).expect("hold the published manifest open");

    StdVfs.write(&temporary, b"generation-2").expect("stage");
    StdVfs.sync(&temporary, SyncKind::Full).expect("flush data");
    StdVfs
        .rename(&temporary, &manifest)
        .expect("publish under a live reader");
    StdVfs
        .sync(directory.path(), SyncKind::Full)
        .expect("flush the namespace");

    assert_eq!(
        StdVfs.read(&manifest).expect("read"),
        b"generation-2",
        "the published name must resolve to the new generation"
    );
    drop(reader);
}

/// The lifetime property compaction depends on: a sealed artifact that a reader
/// has mapped can be unlinked immediately -- the name is gone at once, with no
/// delete-pending window -- while the reader keeps serving the bytes it
/// validated. `await_physical_purge`'s "old paths unlinked" receipt can
/// therefore be honoured literally on Windows.
#[test]
fn a_mapped_artifact_is_unlinked_at_once_and_its_reader_stays_valid() {
    let directory = tempdir().expect("scratch");
    let segment = directory.path().join("segment-0001.zseg");
    let payload: Vec<u8> = (0..16_384_u32).map(|value| (value % 251) as u8).collect();
    StdVfs.write(&segment, &payload).expect("seed");

    // The mapping owns the file the VFS handed it, exactly as `SegmentReader` does.
    let file = StdVfs.open_for_map(&segment).expect("open for map");
    let mapping = zeppelin_embed::sys::windows::TestMapping::open(file, payload.len() as u64)
        .expect("map the sealed segment");

    StdVfs
        .delete(&segment)
        .expect("unlink under a live mapping");
    assert!(
        !segment.try_exists().expect("exists probe"),
        "the path must be unlinked at once, not left delete-pending"
    );
    StdVfs
        .sync(directory.path(), SyncKind::Full)
        .expect("flush the namespace after retirement");

    assert_eq!(
        mapping.bytes(),
        payload.as_slice(),
        "a retained reader must keep the bytes it validated after the unlink"
    );

    // A retired path must not reopen; it must not resurrect a pending file.
    let error = std::fs::File::open(&segment).expect_err("a retired path must not reopen");
    assert_eq!(code_of(&error), Some(ERROR_FILE_NOT_FOUND));
    drop(mapping);
}

/// The other half of the sharing contract: a reader that does not share
/// deletion blocks retirement with a typed sharing error rather than silently
/// succeeding or leaving the engine to guess.
#[test]
fn retirement_is_refused_by_a_reader_that_does_not_share_deletion() {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_SHARE_READ: u32 = 0x0000_0001;

    let directory = tempdir().expect("scratch");
    let segment = directory.path().join("segment-0002.zseg");
    StdVfs.write(&segment, b"retired-but-held").expect("seed");

    let exclusive = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&segment)
        .expect("reader that does not share deletion");

    let error = StdVfs
        .delete(&segment)
        .expect_err("a non-sharing reader must block unlinking");
    assert_eq!(code_of(&error), Some(ERROR_SHARING_VIOLATION));
    assert!(segment.try_exists().expect("exists"));

    drop(exclusive);
    StdVfs
        .delete(&segment)
        .expect("retirement succeeds once the reader is released");
    assert!(!segment.try_exists().expect("exists after delete"));
}

// ---------------------------------------------------------------------------
// Paths.
// ---------------------------------------------------------------------------

/// UTF-16 paths must round-trip without lossy conversion, including spaces and
/// non-ASCII, which a user profile name can easily contain.
#[test]
fn non_ascii_and_spaced_paths_round_trip_through_the_production_vfs() {
    let directory = tempdir().expect("scratch");
    let store = directory
        .path()
        .join("magasin de donn\u{e9}es \u{4e2d}\u{6587}");
    StdVfs
        .ensure_directory(&store, true)
        .expect("create a non-ASCII store directory");
    let manifest = store.join("manifest \u{2013} v1.zman");

    StdVfs
        .write(&manifest, b"unicode-generation")
        .expect("write");
    StdVfs.sync(&manifest, SyncKind::Full).expect("flush data");
    StdVfs
        .sync(&store, SyncKind::Full)
        .expect("flush namespace");

    assert_eq!(StdVfs.read(&manifest).expect("read"), b"unicode-generation");
    assert_eq!(StdVfs.open(&manifest).expect("length"), 18);
}

/// An interior NUL must be rejected before any kernel call rather than
/// silently truncating the path to a different file.
#[test]
fn an_interior_nul_in_a_store_path_is_rejected() {
    let error = StdVfs
        .sync(Path::new("bad\u{0}path"), SyncKind::Full)
        .expect_err("interior NUL must be rejected");
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidFilename
        ),
        "expected a typed path rejection, observed {error:?}"
    );
}

/// A relative spelling of a store path must reach the same directory as its
/// absolute form. The engine keys writer ownership on filesystem identity, not
/// path text, and this is the production-side check of that.
#[test]
fn a_relative_store_path_resolves_to_the_same_directory() {
    let directory = tempdir().expect("scratch");
    let nested = directory.path().join("store");
    std::fs::create_dir_all(&nested).expect("create");
    let mut file = std::fs::File::create(nested.join("marker")).expect("marker");
    file.write_all(b"same-store").expect("write marker");
    drop(file);

    let mixed = nested.join(".").join("marker");
    assert_eq!(
        StdVfs.read(&mixed).expect("read through `.`"),
        b"same-store"
    );

    let slashed =
        std::path::PathBuf::from(nested.to_string_lossy().replace('\\', "/")).join("marker");
    assert_eq!(
        StdVfs.read(&slashed).expect("read through forward slashes"),
        b"same-store"
    );
}
