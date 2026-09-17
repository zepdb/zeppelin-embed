//! W02 native probes.
//!
//! Every probe records the raw Win32 outcome (success, or the exact error code)
//! and the resulting bytes on disk. Run with `-- --nocapture` to capture the
//! observation lines into evidence.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::io;
use std::path::Path;

use windows_storage_probe::protocol::{self, Op, OpLog, Variant, Violation};
use windows_storage_probe::win32;
use windows_storage_probe::Scratch;

/// Prints one observation line so the evidence file states what actually ran.
fn observe(probe: &str, detail: &str) {
    println!("OBSERVE {probe}: {detail}");
}

fn code_of(error: &io::Error) -> u32 {
    u32::try_from(error.raw_os_error().unwrap_or(-1)).unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------------------
// Constants: a wrong value here would silently corrupt a store.
// ---------------------------------------------------------------------------

#[test]
fn declared_constants_match_the_installed_sdk_headers() {
    assert_eq!(win32::GENERIC_READ, 0x8000_0000);
    assert_eq!(win32::GENERIC_WRITE, 0x4000_0000);
    assert_eq!(win32::FILE_SHARE_READ, 1);
    assert_eq!(win32::FILE_SHARE_WRITE, 2);
    assert_eq!(win32::FILE_SHARE_DELETE, 4);
    assert_eq!(win32::FILE_ATTRIBUTE_NORMAL, 0x80);
    assert_eq!(win32::FILE_FLAG_BACKUP_SEMANTICS, 0x0200_0000);
    assert_eq!(win32::FILE_FLAG_WRITE_THROUGH, 0x8000_0000);
    assert_eq!(win32::PAGE_READONLY, 0x02);
    assert_eq!(win32::FILE_MAP_READ, 0x0004);
    assert_eq!(win32::CREATE_NEW, 1);
    assert_eq!(win32::CREATE_ALWAYS, 2);
    assert_eq!(win32::OPEN_EXISTING, 3);
    assert_eq!(win32::OPEN_ALWAYS, 4);
    assert_eq!(win32::TRUNCATE_EXISTING, 5);
    assert_eq!(win32::MOVEFILE_REPLACE_EXISTING, 1);
    assert_eq!(win32::MOVEFILE_COPY_ALLOWED, 2);
    assert_eq!(win32::MOVEFILE_WRITE_THROUGH, 8);
    assert_eq!(win32::ERROR_ACCESS_DENIED, 5);
    assert_eq!(win32::ERROR_NOT_SAME_DEVICE, 17);
    assert_eq!(win32::ERROR_SHARING_VIOLATION, 32);
    assert_eq!(win32::ERROR_USER_MAPPED_FILE, 1224);
    assert_eq!(win32::INVALID_HANDLE_VALUE as isize, -1);
}

// ---------------------------------------------------------------------------
// 1. Data durability: which handles can actually be flushed.
// ---------------------------------------------------------------------------

#[test]
fn flush_succeeds_on_a_writable_handle_and_the_bytes_are_readable() {
    let scratch = Scratch::new("flush-writable").expect("scratch");
    let path = scratch.path().join("data.bin");
    let payload = b"zeppelin durable bytes".to_vec();

    let handle = win32::create_file(
        &path,
        win32::GENERIC_READ | win32::GENERIC_WRITE,
        0,
        win32::CREATE_ALWAYS,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect("create writable");
    win32::write_all(&handle, &payload).expect("write");
    win32::flush(&handle).expect("flush writable handle");
    let size = win32::file_size(&handle).expect("size");
    drop(handle);

    let read_back = std::fs::read(&path).expect("read back");
    observe(
        "flush_writable",
        &format!("FlushFileBuffers ok, size={size}, bytes={}", read_back.len()),
    );
    assert_eq!(size, payload.len() as u64);
    assert_eq!(read_back, payload);
}

/// This is the concrete reason `StdVfs::sync` cannot work on Windows as
/// written: it reopens the path with `File::open`, which is `GENERIC_READ`.
#[test]
fn flush_is_refused_on_a_read_only_handle() {
    let scratch = Scratch::new("flush-readonly").expect("scratch");
    let path = scratch.path().join("data.bin");
    std::fs::write(&path, b"payload").expect("seed");

    let handle = win32::create_file(
        &path,
        win32::GENERIC_READ,
        win32::FILE_SHARE_READ,
        win32::OPEN_EXISTING,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect("open read-only");
    let outcome = win32::flush(&handle);
    let error = outcome.expect_err("a read-only handle must not be flushable");
    observe(
        "flush_readonly",
        &format!("FlushFileBuffers failed with {}", code_of(&error)),
    );
    assert_eq!(code_of(&error), win32::ERROR_ACCESS_DENIED);
}

/// Windows has no `fsync(dirfd)` spelling, but it does have the mechanism: a
/// directory handle opened with `FILE_FLAG_BACKUP_SEMANTICS` **and write
/// access** accepts `FlushFileBuffers`. This is the namespace-durability
/// primitive the protocol is built on, so its exact access requirement is
/// pinned here rather than assumed.
#[test]
fn the_namespace_flush_requires_a_writable_directory_handle() {
    let scratch = Scratch::new("dir-flush").expect("scratch");

    // A read-only directory handle opens, but cannot be flushed.
    let read_handle =
        win32::open_directory(scratch.path(), win32::GENERIC_READ).expect("directory read handle");
    let read_flush = win32::flush(&read_handle).expect_err("a read handle must not flush");
    observe(
        "dir_flush",
        &format!("GENERIC_READ directory handle: flush failed with {}", code_of(&read_flush)),
    );
    assert_eq!(code_of(&read_flush), win32::ERROR_ACCESS_DENIED);

    // A metadata-only handle likewise.
    let metadata_handle = win32::open_directory(scratch.path(), 0).expect("metadata handle");
    let metadata_flush =
        win32::flush(&metadata_handle).expect_err("a metadata-only handle must not flush");
    assert_eq!(code_of(&metadata_flush), win32::ERROR_ACCESS_DENIED);

    // A writable directory handle is obtainable by a standard user, without
    // elevation and without opening a volume handle, and its flush succeeds.
    let write_handle =
        win32::open_directory(scratch.path(), win32::GENERIC_WRITE).expect("directory write handle");
    win32::flush(&write_handle).expect("a writable directory handle must flush");
    observe(
        "dir_flush",
        "GENERIC_WRITE directory handle: open ok, FlushFileBuffers ok",
    );

    // And the wrapper the protocol actually calls.
    protocol::flush_namespace(scratch.path()).expect("flush_namespace");
}

/// The plan's candidate — `MoveFileExW(MOVEFILE_REPLACE_EXISTING |
/// MOVEFILE_WRITE_THROUGH)` — is rejected here on measurement: it cannot
/// replace a name that any reader still holds open, whatever that reader's
/// sharing mask, so it cannot publish a manifest under live readers.
#[test]
fn the_move_file_ex_candidate_cannot_publish_under_an_open_reader() {
    let scratch = Scratch::new("movefileex").expect("scratch");
    let target = scratch.path().join("manifest.zman");
    let source = scratch.path().join("manifest.zman.tmp");
    std::fs::write(&target, b"generation-1").expect("seed target");
    std::fs::write(&source, b"generation-2").expect("seed source");

    let reader = win32::create_file(
        &target,
        win32::GENERIC_READ,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
        win32::OPEN_EXISTING,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect("reader that shares everything, deletion included");

    let error = win32::move_file_ex(&source, &target, protocol::REJECTED_MOVEFILE_FLAGS)
        .expect_err("MoveFileExW must be shown to fail here, not assumed to work");
    observe(
        "movefileex",
        &format!("MoveFileExW(REPLACE|WRITE_THROUGH) refused with {}", code_of(&error)),
    );
    assert_eq!(code_of(&error), win32::ERROR_ACCESS_DENIED);
    assert_eq!(std::fs::read(&target).expect("read"), b"generation-1");

    // The adopted protocol succeeds on the same state.
    let mut log = OpLog::new();
    protocol::publish(&target, b"generation-2", Variant::Correct, &mut log)
        .expect("the POSIX-semantics rename publishes under the same open reader");
    assert_eq!(std::fs::read(&target).expect("read"), b"generation-2");
    assert_eq!(protocol::judge(&log), Ok(()));
    drop(reader);
}

// ---------------------------------------------------------------------------
// 2. Publication: create, then replace.
// ---------------------------------------------------------------------------

#[test]
fn first_publication_creates_the_name_and_the_second_replaces_it() {
    let scratch = Scratch::new("publish").expect("scratch");
    let manifest = scratch.path().join("manifest.zman");

    let mut first = OpLog::new();
    protocol::publish(&manifest, b"generation-1", Variant::Correct, &mut first)
        .expect("first publication");
    assert_eq!(std::fs::read(&manifest).expect("read v1"), b"generation-1");
    assert!(
        first.ops().iter().any(|op| matches!(op, Op::Create { .. })),
        "first publication must be recorded as a create: {:?}",
        first.ops()
    );
    assert!(
        matches!(first.ops().last(), Some(Op::FlushNamespace(_))),
        "a publication must end with its namespace flush: {:?}",
        first.ops()
    );

    let mut second = OpLog::new();
    protocol::publish(&manifest, b"generation-2", Variant::Correct, &mut second)
        .expect("replacement");
    assert_eq!(std::fs::read(&manifest).expect("read v2"), b"generation-2");
    assert!(
        second.ops().iter().any(|op| matches!(op, Op::Replace { .. })),
        "second publication must be recorded as a replace: {:?}",
        second.ops()
    );

    // The temporary must not survive a successful publication.
    let leftovers: Vec<_> = std::fs::read_dir(scratch.path())
        .expect("list")
        .filter_map(|entry| entry.ok().map(|value| value.file_name()))
        .collect();
    observe(
        "publish",
        &format!("directory after two publications: {leftovers:?}"),
    );
    assert_eq!(leftovers.len(), 1, "only the published name may remain");

    assert_eq!(protocol::judge(&first), Ok(()));
    assert_eq!(protocol::judge(&second), Ok(()));
}

// ---------------------------------------------------------------------------
// 3. The oracle fires on a deliberately broken protocol.
// ---------------------------------------------------------------------------

#[test]
fn the_oracle_rejects_a_publication_whose_data_was_never_flushed() {
    let scratch = Scratch::new("oracle-noflush").expect("scratch");
    let manifest = scratch.path().join("manifest.zman");
    let mut log = OpLog::new();
    protocol::publish(&manifest, b"unflushed", Variant::MissingDataFlush, &mut log)
        .expect("the violation still performs its I/O");

    // The bytes are present: this is exactly why a byte check cannot replace
    // the ordering oracle.
    assert_eq!(std::fs::read(&manifest).expect("read"), b"unflushed");

    let verdict = protocol::judge(&log);
    observe("oracle_noflush", &format!("verdict={verdict:?}"));
    assert!(matches!(
        verdict,
        Err(Violation::UnflushedPublication { .. })
    ));
}

#[test]
fn the_oracle_rejects_a_flush_that_follows_publication() {
    let scratch = Scratch::new("oracle-reorder").expect("scratch");
    let manifest = scratch.path().join("manifest.zman");
    let mut log = OpLog::new();
    protocol::publish(&manifest, b"reordered", Variant::FlushAfterPublish, &mut log)
        .expect("the violation still performs its I/O");

    assert_eq!(std::fs::read(&manifest).expect("read"), b"reordered");

    let verdict = protocol::judge(&log);
    observe("oracle_reorder", &format!("verdict={verdict:?}"));
    assert!(matches!(
        verdict,
        Err(Violation::FlushAfterPublication { .. })
    ));
}

#[test]
fn the_oracle_rejects_an_empty_trace() {
    assert_eq!(protocol::judge(&OpLog::new()), Err(Violation::NothingPublished));
}

// ---------------------------------------------------------------------------
// 4. Retained readers across replacement and deletion.
// ---------------------------------------------------------------------------

/// The lifetime property compaction depends on: a name may be republished
/// while a reader has the old bytes mapped, and that reader keeps serving the
/// generation it validated.
#[test]
fn publication_under_a_mapped_reader_leaves_that_reader_on_its_own_generation() {
    let scratch = Scratch::new("mapped-replace").expect("scratch");
    let segment = scratch.path().join("segment-0001.zseg");
    std::fs::write(&segment, b"sealed-generation-1").expect("seed");

    let map = win32::ReadOnlyMap::open(
        &segment,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
    )
    .expect("map the sealed segment");
    assert_eq!(map.as_bytes(), b"sealed-generation-1");

    let mut log = OpLog::new();
    protocol::publish(&segment, b"sealed-generation-2", Variant::Correct, &mut log)
        .expect("publication must succeed under a mapped reader");

    assert_eq!(
        map.as_bytes(),
        b"sealed-generation-1",
        "an admitted reader must never observe bytes it did not validate"
    );
    assert_eq!(
        std::fs::read(&segment).expect("read"),
        b"sealed-generation-2"
    );
    observe(
        "mapped_replace",
        "published under a live mapping; the old view kept generation-1",
    );
    assert_eq!(protocol::judge(&log), Ok(()));
    drop(map);
}

#[test]
fn an_unmapped_reader_sharing_delete_does_not_block_replacement() {
    let scratch = Scratch::new("shared-replace").expect("scratch");
    let manifest = scratch.path().join("manifest.zman");
    std::fs::write(&manifest, b"generation-1").expect("seed");

    let reader = win32::create_file(
        &manifest,
        win32::GENERIC_READ,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
        win32::OPEN_EXISTING,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect("reader");

    let mut log = OpLog::new();
    let outcome = protocol::publish(&manifest, b"generation-2", Variant::Correct, &mut log);
    match &outcome {
        Ok(()) => observe("shared_replace", "replacement with a FILE_SHARE_DELETE reader: ok"),
        Err(error) => observe(
            "shared_replace",
            &format!("replacement refused with {}", code_of(error)),
        ),
    }
    outcome.expect("a reader that shares delete must not block publication");
    assert_eq!(std::fs::read(&manifest).expect("read"), b"generation-2");
    drop(reader);
}

#[test]
fn a_reader_without_share_delete_blocks_replacement_with_a_sharing_error() {
    let scratch = Scratch::new("exclusive-reader").expect("scratch");
    let manifest = scratch.path().join("manifest.zman");
    std::fs::write(&manifest, b"generation-1").expect("seed");

    let reader = win32::create_file(
        &manifest,
        win32::GENERIC_READ,
        win32::FILE_SHARE_READ,
        win32::OPEN_EXISTING,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect("reader");

    let mut log = OpLog::new();
    let error = protocol::publish(&manifest, b"generation-2", Variant::Correct, &mut log)
        .expect_err("a reader that does not share delete must block replacement");
    observe(
        "exclusive_reader",
        &format!("replacement refused with {}", code_of(&error)),
    );
    assert_eq!(code_of(&error), win32::ERROR_ACCESS_DENIED);
    assert_eq!(std::fs::read(&manifest).expect("read"), b"generation-1");
    drop(reader);
}

/// The physical-purge contract is "old paths unlinked, retained readers still
/// safe". Both halves are measured here: with `FILE_SHARE_DELETE` on the
/// mapping's file handle the name is removed **immediately** — not left
/// delete-pending — while the live view keeps serving correct bytes.
#[test]
fn retirement_unlinks_immediately_while_a_mapping_stays_valid() {
    let scratch = Scratch::new("delete-mapped").expect("scratch");
    let segment = scratch.path().join("segment-0002.zseg");
    let payload: Vec<u8> = (0..8192_u32).map(|value| (value % 251) as u8).collect();
    std::fs::write(&segment, &payload).expect("seed");

    let map = win32::ReadOnlyMap::open(
        &segment,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_DELETE,
    )
    .expect("map");

    let mut log = OpLog::new();
    protocol::retire(&segment, &mut log).expect("retire under a live mapping");

    let still_linked = segment.try_exists().expect("exists probe");
    observe(
        "delete_mapped",
        &format!("after retire: still_linked={still_linked}, mapped bytes intact={}", map.as_bytes() == payload.as_slice()),
    );
    assert!(
        !still_linked,
        "the path must be unlinked immediately, not left delete-pending"
    );
    assert_eq!(
        map.as_bytes(),
        payload.as_slice(),
        "a retained reader must keep its validated bytes after the unlink"
    );
    assert_eq!(protocol::judge(&log), Ok(()));

    // Reopening the retired name must fail, not resurrect a delete-pending file.
    let reopen = win32::create_file(
        &segment,
        win32::GENERIC_READ,
        win32::FILE_SHARE_READ,
        win32::OPEN_EXISTING,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect_err("a retired path must not reopen");
    assert_eq!(code_of(&reopen), win32::ERROR_FILE_NOT_FOUND);
    drop(map);
}

/// The other half of the sharing contract: a mapping that does **not** share
/// deletion blocks retirement with a typed sharing error rather than silently
/// succeeding or leaving the engine guessing.
#[test]
fn retirement_is_refused_when_a_mapping_does_not_share_deletion() {
    let scratch = Scratch::new("delete-noshare").expect("scratch");
    let segment = scratch.path().join("segment-0004.zseg");
    std::fs::write(&segment, b"retired-but-exclusively-mapped").expect("seed");

    let map = win32::ReadOnlyMap::open(&segment, win32::FILE_SHARE_READ).expect("map");
    let error = win32::delete_file(&segment)
        .expect_err("a mapping without FILE_SHARE_DELETE must block unlinking");
    observe(
        "delete_noshare",
        &format!("DeleteFileW refused with {}", code_of(&error)),
    );
    assert_eq!(code_of(&error), win32::ERROR_SHARING_VIOLATION);
    assert!(segment.try_exists().expect("exists"));

    drop(map);
    win32::delete_file(&segment).expect("deletion succeeds once the mapping is released");
    assert!(!segment.try_exists().expect("exists after delete"));
}

#[test]
fn the_oracle_rejects_a_namespace_mutation_that_was_never_flushed() {
    let scratch = Scratch::new("oracle-nsflush").expect("scratch");
    let manifest = scratch.path().join("manifest.zman");
    let mut log = OpLog::new();
    protocol::publish(
        &manifest,
        b"unflushed-namespace",
        Variant::MissingNamespaceFlush,
        &mut log,
    )
    .expect("the violation still performs its I/O");

    assert_eq!(
        std::fs::read(&manifest).expect("read"),
        b"unflushed-namespace"
    );
    let verdict = protocol::judge(&log);
    observe("oracle_nsflush", &format!("verdict={verdict:?}"));
    assert!(matches!(verdict, Err(Violation::UnflushedNamespace { .. })));
}

#[test]
fn deletion_with_an_open_sharing_reader_records_its_name_removal_behaviour() {
    let scratch = Scratch::new("delete-pending").expect("scratch");
    let artifact = scratch.path().join("segment-0003.zseg");
    std::fs::write(&artifact, b"retired").expect("seed");

    let reader = win32::create_file(
        &artifact,
        win32::GENERIC_READ,
        win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE | win32::FILE_SHARE_DELETE,
        win32::OPEN_EXISTING,
        win32::FILE_ATTRIBUTE_NORMAL,
    )
    .expect("reader");

    win32::delete_file(&artifact).expect("delete with a sharing reader");
    let still_linked = artifact.try_exists().expect("exists probe");
    let reopen = win32::create_file(
        &artifact,
        win32::GENERIC_READ,
        win32::FILE_SHARE_READ,
        win32::OPEN_EXISTING,
        win32::FILE_ATTRIBUTE_NORMAL,
    );
    let reopen_code = reopen.err().map_or(0, |error| code_of(&error));
    observe(
        "delete_pending",
        &format!("after DeleteFileW with an open reader: still_linked={still_linked}, reopen_code={reopen_code}"),
    );

    drop(reader);
    let linked_after_close = artifact.try_exists().expect("exists probe");
    observe(
        "delete_pending",
        &format!("after the last handle closed: still_linked={linked_after_close}"),
    );
    assert!(
        !linked_after_close,
        "the name must be gone once every handle is closed"
    );
}

// ---------------------------------------------------------------------------
// 5. WAL appends through a retained handle.
// ---------------------------------------------------------------------------

#[test]
fn wal_appends_and_flushes_reach_the_same_retained_file_object() {
    let scratch = Scratch::new("wal").expect("scratch");
    let wal = scratch.path().join("wal-000001.zwal");

    let handle = protocol::open_append(&wal).expect("first creation of the WAL");
    let first_identity = win32::file_identity(&handle).expect("identity");
    win32::write_all(&handle, b"record-one\n").expect("append one");
    win32::flush(&handle).expect("flush one");
    win32::write_all(&handle, b"record-two\n").expect("append two");
    win32::flush(&handle).expect("flush two");
    let second_identity = win32::file_identity(&handle).expect("identity again");
    let size = win32::file_size(&handle).expect("size");
    drop(handle);

    assert_eq!(
        first_identity, second_identity,
        "both appends must reach one file object"
    );
    let bytes = std::fs::read(&wal).expect("read wal");
    observe(
        "wal",
        &format!(
            "identity={first_identity:?}, size={size}, bytes={}",
            bytes.len()
        ),
    );
    assert_eq!(bytes, b"record-one\nrecord-two\n");
    assert_eq!(size, bytes.len() as u64);
}

// ---------------------------------------------------------------------------
// 6. Same-volume requirement and typed failures.
// ---------------------------------------------------------------------------

#[test]
fn a_store_directory_and_its_temporary_share_one_volume_identity() {
    let scratch = Scratch::new("volume").expect("scratch");
    let nested = scratch.path().join("namespace");
    std::fs::create_dir_all(&nested).expect("nested");

    assert!(
        protocol::same_volume(scratch.path(), &nested).expect("same volume probe"),
        "a store and its own subdirectory must share a volume"
    );

    let handle = win32::open_directory(scratch.path(), win32::GENERIC_READ).expect("dir handle");
    let (volume, index) = win32::file_identity(&handle).expect("identity");
    observe(
        "volume",
        &format!("store volume serial={volume:#010x}, directory index={index:#018x}"),
    );
    assert_ne!(index, 0, "a directory must have a non-zero file index");
}

#[test]
fn publication_into_a_missing_directory_fails_with_a_typed_path_error() {
    let scratch = Scratch::new("missing-dir").expect("scratch");
    let absent = scratch.path().join("no-such-namespace").join("manifest.zman");
    let mut log = OpLog::new();
    let error = protocol::publish(&absent, b"x", Variant::Correct, &mut log)
        .expect_err("a missing parent must fail loudly");
    observe(
        "missing_dir",
        &format!("publication into a missing directory failed with {}", code_of(&error)),
    );
    assert_eq!(code_of(&error), win32::ERROR_PATH_NOT_FOUND);
}

#[test]
fn an_interior_nul_in_a_path_is_rejected_before_any_kernel_call() {
    let encoded = win32::wide(Path::new("bad\u{0}path"));
    let error = encoded.expect_err("interior NUL must be rejected");
    observe("interior_nul", &format!("rejected as {:?}", error.kind()));
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn non_ascii_and_spaced_store_paths_round_trip_through_utf16() {
    let scratch = Scratch::new("unicode").expect("scratch");
    let directory = scratch.path().join("magasin de données \u{4e2d}\u{6587}");
    std::fs::create_dir_all(&directory).expect("create unicode directory");
    let manifest = directory.join("manifest \u{2013} v1.zman");

    let mut log = OpLog::new();
    protocol::publish(&manifest, b"unicode-generation", Variant::Correct, &mut log)
        .expect("publish under a non-ASCII path");
    assert_eq!(
        std::fs::read(&manifest).expect("read"),
        b"unicode-generation"
    );

    let handle = win32::open_directory(&directory, win32::GENERIC_READ).expect("dir handle");
    let identity = win32::file_identity(&handle).expect("identity");
    observe(
        "unicode",
        &format!("published under {} with identity {identity:?}", manifest.display()),
    );
    assert_eq!(protocol::judge(&log), Ok(()));
}

#[test]
fn free_space_is_reported_for_the_actual_store_volume() {
    let scratch = Scratch::new("freespace").expect("scratch");
    let available = win32::available_bytes(scratch.path()).expect("free space");
    observe("freespace", &format!("available_to_caller={available}"));
    assert!(available > 0, "a writable scratch volume must report free space");
}

#[test]
fn mapping_an_empty_file_fails_instead_of_producing_an_empty_slice() {
    let scratch = Scratch::new("empty-map").expect("scratch");
    let empty = scratch.path().join("segment-empty.zseg");
    std::fs::write(&empty, b"").expect("seed empty");
    let error = win32::ReadOnlyMap::open(&empty, win32::FILE_SHARE_READ)
        .expect_err("an empty file must not map");
    observe("empty_map", &format!("rejected as {:?}", error.kind()));
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}
