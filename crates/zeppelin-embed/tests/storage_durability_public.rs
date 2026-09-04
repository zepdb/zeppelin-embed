#![allow(clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(unix)]
use std::process::Command;

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, StorageFaultController,
    StorageFaultPlan, StorageFaultReceipt, StorageReceiptObserved, StorageTestFault, Store,
    StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::layout::{REGION_ENTRY_LEN, RegionKind};
use zeppelin_embed::vfs::{CountingVfs, StdVfs};

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write storage artifact fixture");
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read storage artifact fixture")
}

fn directory_bytes(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = std::fs::read_dir(path)
        .expect("list storage artifact fixture")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                read(&entry.path()),
            )
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn ingest_one(store: &Store, doc_id: u128) {
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(doc_id), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("ingest fixture row");
}

fn active_fixture() -> tempfile::TempDir {
    let directory = tempdir().expect("active fixture directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open active fixture");
    ingest_one(&store, 91);
    store.close().expect("close active fixture");
    directory
}

fn sealed_fixture() -> tempfile::TempDir {
    let directory = tempdir().expect("sealed fixture directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open sealed fixture");
    ingest_one(&store, 92);
    store.seal().expect("seal fixture");
    store.close().expect("close sealed fixture");
    directory
}

fn segment_path(directory: &Path) -> std::path::PathBuf {
    std::fs::read_dir(directory)
        .expect("list store")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        })
        .expect("sealed segment path")
}

fn segment_id(path: &Path) -> zeppelin_embed::segment::SegmentId {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 segment filename");
    let hex = name
        .strip_prefix("segment-")
        .and_then(|name| name.strip_suffix(".zseg"))
        .expect("canonical segment filename");
    assert_eq!(hex.len(), 32, "segment identity width");
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&hex[offset..offset + 2], 16).expect("segment identity byte");
    }
    zeppelin_embed::segment::SegmentId::from_bytes(bytes)
}

fn segment_id_from_hex(hex: &str) -> zeppelin_embed::segment::SegmentId {
    assert_eq!(hex.len(), 32, "segment identity width");
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&hex[offset..offset + 2], 16).expect("segment identity byte");
    }
    zeppelin_embed::segment::SegmentId::from_bytes(bytes)
}

fn storage_controller(fault: StorageTestFault, plan: StorageFaultPlan) -> StorageFaultController {
    StorageFaultController::new(fault, plan)
}

fn dependencies(controller: StorageFaultController) -> StoreTestDependencies {
    StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_storage_fault_controller(controller)
}

fn assert_receipt(
    controller: &StorageFaultController,
    operation: &str,
    fault: &str,
    site: &str,
) -> StorageFaultReceipt {
    let receipt = controller.take_receipt().unwrap_or_else(|| {
        panic!("{fault} expected 1 receipt at {site}, got 0");
    });
    assert_eq!(receipt.campaign(), "storage-durability");
    assert_eq!(receipt.operation(), operation);
    assert_eq!(receipt.fault(), fault);
    assert_eq!(receipt.site(), site);
    assert_eq!(receipt.cardinality(), 1);
    let observed_artifact = match receipt.observed() {
        StorageReceiptObserved::WalHeader { artifact, .. }
        | StorageReceiptObserved::WalRecord { artifact, .. }
        | StorageReceiptObserved::WalAppend { artifact, .. }
        | StorageReceiptObserved::SegmentChecksum { artifact, .. }
        | StorageReceiptObserved::Format { artifact, .. }
        | StorageReceiptObserved::Omission { artifact, .. } => artifact.as_str(),
        StorageReceiptObserved::ManifestRename { committed, .. } => committed.as_str(),
    };
    assert_eq!(observed_artifact, receipt.plan().artifact());
    receipt
}

#[test]
fn storage_torn_wal_header_can_fire() {
    let directory = active_fixture();
    let wal = directory.path().join("wal.ze");
    let mut bytes = read(&wal);
    bytes.truncate(20);
    write(&wal, &bytes);
    let controller = storage_controller(
        StorageTestFault::TornWalHeader,
        StorageFaultPlan::new(0, "wal.ze").with_offset(20),
    );
    let error = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .err()
    .expect("torn WAL header must refuse public open");
    assert!(matches!(
        error,
        zeppelin_embed::lifecycle::StoreError::WalRecovery(
            zeppelin_embed::wal::WalRecoveryError::InvalidHeader(
                zeppelin_embed::wal::header::WalHeaderError::Truncated {
                    needed: 40,
                    available: 20
                }
            )
        )
    ));
    let receipt = assert_receipt(
        &controller,
        "wal-prefix",
        "torn-wal-header",
        "WalOpen.HeaderValidation",
    );
    assert_eq!(receipt.plan().offset(), Some(20));
    assert_eq!(
        receipt.observed(),
        &StorageReceiptObserved::WalHeader {
            artifact: "wal.ze".to_owned(),
            reason: zeppelin_embed::wal::header::WalHeaderError::Truncated {
                needed: 40,
                available: 20,
            },
        }
    );
}

#[test]
fn sealed_store_torn_wal_header_is_not_masked_by_manifest_coverage() {
    let directory = sealed_fixture();
    let wal = directory.path().join("wal.ze");
    let mut bytes = read(&wal);
    bytes.truncate(20);
    write(&wal, &bytes);
    let controller = storage_controller(
        StorageTestFault::TornWalHeader,
        StorageFaultPlan::new(17, "wal.ze").with_offset(20),
    );

    let error = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::read_only(),
        dependencies(controller.clone()),
    )
    .err()
    .expect("sealed torn WAL header must refuse public open");

    assert!(matches!(
        error,
        zeppelin_embed::lifecycle::StoreError::WalRecovery(
            zeppelin_embed::wal::WalRecoveryError::InvalidHeader(
                zeppelin_embed::wal::header::WalHeaderError::Truncated {
                    needed: 40,
                    available: 20
                }
            )
        )
    ));
    let receipt = assert_receipt(
        &controller,
        "wal-prefix",
        "torn-wal-header",
        "WalOpen.HeaderValidation",
    );
    assert_eq!(receipt.plan().op_index(), 17);
    assert_eq!(receipt.plan().offset(), Some(20));
}

#[test]
fn storage_torn_wal_body_can_fire() {
    let directory = active_fixture();
    let wal = directory.path().join("wal.ze");
    let mut bytes = read(&wal);
    bytes.truncate(bytes.len().saturating_sub(12));
    write(&wal, &bytes);
    let controller = storage_controller(
        StorageTestFault::TornWalBody,
        StorageFaultPlan::new(0, "wal.ze")
            .with_offset(u64::try_from(bytes.len()).expect("torn WAL length fits u64")),
    );
    let error = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .err()
    .expect("torn WAL body must refuse public open");
    assert!(matches!(
        error,
        zeppelin_embed::lifecycle::StoreError::WalRecovery(
            zeppelin_embed::wal::WalRecoveryError::CorruptAt { offset: 40, .. }
        )
    ));
    let receipt = assert_receipt(
        &controller,
        "wal-prefix",
        "torn-wal-body",
        "WalOpen.RecordValidation",
    );
    assert_eq!(
        receipt.plan().offset(),
        Some(u64::try_from(bytes.len()).expect("torn WAL length fits u64"))
    );
    let StorageReceiptObserved::WalRecord {
        artifact,
        offset,
        reason,
    } = receipt.observed()
    else {
        panic!("torn WAL receipt had wrong typed effect");
    };
    assert_eq!(artifact, "wal.ze");
    assert_eq!(*offset, 40);
    assert!(matches!(
        reason,
        zeppelin_embed::wal::replay::CorruptionReason::Record {
            error: zeppelin_embed::wal::record::RecordError::BodyTruncated {
                available,
                ..
            },
            ..
        } if *available == bytes.len() - 40
    ));
}

#[test]
fn storage_torn_wal_checksum_can_fire() {
    let directory = active_fixture();
    let wal = directory.path().join("wal.ze");
    let mut bytes = read(&wal);
    let checksum_offset = bytes
        .len()
        .checked_sub(1)
        .expect("WAL checksum byte offset");
    let last = bytes.last_mut().expect("WAL checksum byte");
    *last ^= 0x80;
    write(&wal, &bytes);
    let controller = storage_controller(
        StorageTestFault::TornWalChecksum,
        StorageFaultPlan::new(0, "wal.ze")
            .with_offset(u64::try_from(checksum_offset).expect("checksum offset fits u64")),
    );
    let error = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .err()
    .expect("bad WAL checksum must refuse public open");
    assert!(matches!(
        error,
        zeppelin_embed::lifecycle::StoreError::WalRecovery(
            zeppelin_embed::wal::WalRecoveryError::CorruptAt { offset: 40, .. }
        )
    ));
    let receipt = assert_receipt(
        &controller,
        "wal-prefix",
        "torn-wal-checksum",
        "WalOpen.RecordChecksum",
    );
    assert_eq!(
        receipt.plan().offset(),
        Some(u64::try_from(checksum_offset).expect("checksum offset fits u64"))
    );
    let StorageReceiptObserved::WalRecord {
        artifact,
        offset,
        reason,
    } = receipt.observed()
    else {
        panic!("checksum receipt had wrong typed effect");
    };
    assert_eq!(artifact, "wal.ze");
    assert_eq!(*offset, 40);
    assert!(matches!(
        reason,
        zeppelin_embed::wal::replay::CorruptionReason::Record {
            error: zeppelin_embed::wal::record::RecordError::ChecksumMismatch {
                expected,
                actual,
                ..
            },
            ..
        } if expected != actual
    ));
}

#[test]
fn storage_post_commit_error_can_fire() {
    let directory = tempdir().expect("post-commit directory");
    let controller = storage_controller(
        StorageTestFault::PostCommitError,
        StorageFaultPlan::new(0, "wal.ze"),
    );
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .expect("open post-commit fixture");
    let error = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(93), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect_err("post-commit injection must make the first caller ambiguous");
    assert!(matches!(
        error,
        IngestError::Store(zeppelin_embed::lifecycle::StoreError::WalWrite(
            zeppelin_embed::wal::WalWriteError::Failed { kind, ref detail }
        )) if kind == std::io::ErrorKind::Other
            && detail.as_ref() == "scheduled post-commit error after inner append"
    ));
    let receipt = assert_receipt(
        &controller,
        "retry",
        "post-commit-error",
        "WalCommit.AppendAfterInnerSuccess",
    );
    let StorageReceiptObserved::WalAppend {
        artifact,
        encoded_len,
        first_seq,
        last_seq,
        inner_append_completed,
        caller_saw_error,
    } = receipt.observed()
    else {
        panic!("post-commit receipt had wrong typed effect");
    };
    assert_eq!(artifact, "wal.ze");
    assert_eq!(
        *encoded_len,
        u64::try_from(read(&directory.path().join("wal.ze")).len() - 40)
            .expect("encoded WAL length fits u64")
    );
    assert_eq!((*first_seq, *last_seq), (1, 1));
    assert!(*inner_append_completed);
    assert!(*caller_saw_error);
    store.close().expect("close ambiguous writer");
    drop(store);
    let wal_before_retry = read(&directory.path().join("wal.ze"));
    let reopened = Store::open(directory.path(), OpenOptions::default())
        .expect("reopen committed ambiguous write");
    let generation = reopened.snapshot().expect("reopened snapshot").generation();
    let retry = reopened
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(93), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("identical retry converges");
    assert_eq!(retry.seq().get(), 1);
    assert_eq!(retry.generation(), generation);
    assert_eq!(
        read(&directory.path().join("wal.ze")),
        wal_before_retry,
        "retry appended a duplicate WAL record"
    );
    assert_eq!(
        reopened.stats().expect("reopened stats").active_row_count,
        1
    );
    reopened.close().expect("close retried store");
}

#[test]
fn retry_is_byte_idempotent_active_and_sealed() {
    let directory = tempdir().expect("retry directory");
    let request = || {
        IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(0x17), Revision::new(9)),
            vec![0.25, -0.75],
        )])
    };
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open retry store");
    let first = store.ingest(request()).expect("first active mutation");
    assert_eq!(first.seq().get(), 1);
    assert_eq!(first.generation(), 1);
    let active_retry = store.ingest(request()).expect("active same-handle retry");
    assert_eq!(active_retry.seq(), first.seq());
    assert_eq!(active_retry.generation(), first.generation());
    assert_eq!(
        store.stats().expect("active retry stats").active_row_count,
        1
    );
    store.seal().expect("seal retry fixture");
    store.close().expect("close sealed retry fixture");

    let wal_before = read(&directory.path().join("wal.ze"));
    let reopened =
        Store::open(directory.path(), OpenOptions::default()).expect("reopen sealed retry fixture");
    let sealed_generation = reopened.snapshot().expect("sealed snapshot").generation();
    let sealed_retry = reopened
        .ingest(request())
        .expect("sealed/reopened equal retry");
    assert_eq!(sealed_retry.seq(), first.seq());
    assert_eq!(sealed_retry.generation(), sealed_generation);
    assert_eq!(
        reopened
            .snapshot()
            .expect("sealed retry snapshot")
            .segments()[0]
            .meta()
            .row_count,
        1
    );
    reopened.close().expect("close sealed retry handle");
    assert_eq!(
        read(&directory.path().join("wal.ze")),
        wal_before,
        "sealed/reopened retry appended a second WAL record"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess-only helper selected by the parent publication tests"]
fn storage_manifest_abort_child_helper() {
    let directory = std::env::var_os("ZE_STORAGE_ABORT_DIRECTORY")
        .map(std::path::PathBuf::from)
        .expect("child storage directory");
    let fault = match std::env::var("ZE_STORAGE_ABORT_FAULT")
        .expect("child storage fault")
        .as_str()
    {
        "pre" => StorageTestFault::ManifestPreRename,
        "post" => StorageTestFault::ManifestPostRename,
        unexpected => panic!("unexpected child storage fault {unexpected}"),
    };
    let ack_fd = std::env::var("ZE_STORAGE_ABORT_ACK_FD")
        .expect("child acknowledgment fd")
        .parse::<i32>()
        .expect("numeric child acknowledgment fd");
    let planned_segment = segment_id_from_hex(
        &std::env::var("ZE_STORAGE_ABORT_SEGMENT_ID").expect("child planned segment id"),
    );
    // SAFETY: the parent passes ownership of this inherited descriptor to this
    // subprocess and closes its own copy immediately after spawning us.
    let ack = unsafe { UnixStream::from_raw_fd(ack_fd) };
    let controller = storage_controller(
        fault,
        StorageFaultPlan::new(0, "manifest.ze").with_segment(planned_segment),
    )
    .with_child_abort_ack(ack);
    let store = Store::open_with_test_dependencies(
        &directory,
        OpenOptions::default(),
        dependencies(controller),
    )
    .expect("child opens active fixture");
    store
        .seal()
        .expect("manifest crash checkpoint must abort before seal returns");
}

#[cfg(unix)]
fn manifest_abort_child(
    directory: &Path,
    fault: &str,
    planned_segment: zeppelin_embed::segment::SegmentId,
) -> (std::process::ExitStatus, String) {
    let (mut parent_ack, child_ack) = UnixStream::pair().expect("publication acknowledgment pipe");
    let child_fd = child_ack.as_raw_fd();
    // SAFETY: `child_fd` is live for this call. Clearing only `FD_CLOEXEC`
    // deliberately transfers a duplicate into the spawned helper process.
    let current_flags = unsafe { libc::fcntl(child_fd, libc::F_GETFD) };
    assert!(current_flags >= 0, "read acknowledgment descriptor flags");
    // SAFETY: `child_fd` remains live and the flags came from `F_GETFD` above.
    let updated =
        unsafe { libc::fcntl(child_fd, libc::F_SETFD, current_flags & !libc::FD_CLOEXEC) };
    assert_eq!(updated, 0, "make acknowledgment descriptor inheritable");

    let mut descriptor_limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: the pointer names writable storage for one `rlimit` result.
    let limit_result =
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, descriptor_limit.as_mut_ptr()) };
    assert_eq!(limit_result, 0, "read process descriptor limit");
    // SAFETY: `getrlimit` succeeded and initialized the value above.
    let descriptor_limit = unsafe { descriptor_limit.assume_init() }.rlim_cur;
    let mut command = Command::new(std::env::current_exe().expect("storage test executable"));
    command
        .arg("--exact")
        .arg("storage_manifest_abort_child_helper")
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("ZE_STORAGE_ABORT_DIRECTORY", directory)
        .env("ZE_STORAGE_ABORT_FAULT", fault)
        .env("ZE_STORAGE_ABORT_SEGMENT_ID", planned_segment.to_string())
        .env("ZE_STORAGE_ABORT_ACK_FD", child_fd.to_string());
    // SAFETY: the closure calls only async-signal-safe `close(2)` between fork
    // and exec. Closing unrelated inherited descriptors prevents parallel
    // Store writer locks from leaking into the abort helper.
    unsafe {
        command.pre_exec(move || {
            for descriptor in 3..descriptor_limit {
                let Ok(descriptor) = i32::try_from(descriptor) else {
                    break;
                };
                if descriptor != child_fd {
                    let _ = libc::close(descriptor);
                }
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn publication child");
    drop(child_ack);
    let status = child.wait().expect("wait for publication child");
    let mut receipt = String::new();
    parent_ack
        .read_to_string(&mut receipt)
        .expect("read publication receipt");
    (status, receipt)
}

#[cfg(unix)]
fn assert_manifest_abort(fault: &str, expected_site: &str, rename_performed: bool) {
    let directory = active_fixture();
    let manifest_path = directory.path().join("manifest.ze");
    let manifest_before = std::fs::read(&manifest_path).ok();
    let planned_segment = zeppelin_embed::segment::SegmentId::from_bytes([
        0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1,
    ]);
    let (status, receipt) = manifest_abort_child(directory.path(), fault, planned_segment);
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "publication child did not abort: {status:?}"
    );
    let fault_key = if fault == "pre" {
        "manifest-pre-rename-crash"
    } else {
        "manifest-post-rename-crash"
    };
    assert_eq!(
        receipt,
        format!(
            "campaign=storage-durability|operation=publication|fault={fault_key}|site={expected_site}|op_index=0|cardinality=1|artifact=manifest.ze|temporary=.manifest.ze.tmp|committed=manifest.ze|rename_performed={rename_performed}|new_segment_final=true|directory_sync_returned=false\n"
        ),
        "child acknowledgment was not the exact production receipt"
    );

    let manifest_after = std::fs::read(&manifest_path).ok();
    let raw_segments = std::fs::read_dir(directory.path())
        .expect("list post-abort artifacts")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        })
        .count();
    assert_eq!(raw_segments, 1, "child did not finish exactly one segment");
    if rename_performed {
        assert!(manifest_after.is_some(), "post-rename manifest is absent");
        assert_ne!(
            manifest_after, manifest_before,
            "post-rename manifest stayed old"
        );
    } else {
        assert_eq!(
            manifest_after, manifest_before,
            "pre-rename manifest changed"
        );
    }

    let reopened = Store::open(directory.path(), OpenOptions::default())
        .expect("publication state reopens publicly");
    let expected_segments = usize::from(rename_performed);
    assert_eq!(
        reopened
            .snapshot()
            .expect("publication snapshot")
            .segments()
            .len(),
        expected_segments
    );
    assert_eq!(
        reopened
            .stats()
            .expect("publication stats")
            .active_row_count,
        u64::from(!rename_performed)
    );
    reopened.close().expect("close publication reopen");
}

#[cfg(unix)]
#[test]
fn storage_manifest_pre_rename_crash_can_fire() {
    assert_manifest_abort("pre", "ManifestCommit.BeforeRename", false);
}

#[cfg(unix)]
#[test]
fn storage_manifest_post_rename_crash_can_fire() {
    assert_manifest_abort("post", "ManifestCommit.AfterRename", true);
}

#[cfg(unix)]
#[test]
fn storage_manifest_receipt_checks_the_planned_new_segment() {
    let directory = sealed_fixture();
    let store =
        Store::open(directory.path(), OpenOptions::default()).expect("reopen sealed fixture");
    ingest_one(&store, 93);
    store.close().expect("close active plus sealed fixture");
    let absent = zeppelin_embed::segment::SegmentId::from_bytes([0xff; 16]);
    assert!(!directory.path().join(absent.file_name()).exists());
    let (status, receipt) = manifest_abort_child(directory.path(), "pre", absent);
    assert_eq!(status.signal(), Some(libc::SIGABRT));
    assert!(
        receipt.contains("new_segment_final=false"),
        "receipt accepted an unrelated final segment: {receipt}"
    );
}

#[test]
fn storage_corrupt_segment_region_can_fire() {
    let directory = sealed_fixture();
    let segment = segment_path(directory.path());
    let mut bytes = read(&segment);
    let region_count = u16::from_le_bytes([bytes[52], bytes[53]]) as usize;
    let mut payload_offset = None;
    for index in 0..region_count {
        let entry = 64 + index * REGION_ENTRY_LEN;
        let kind = u16::from_le_bytes([bytes[entry], bytes[entry + 1]]);
        if kind == RegionKind::VectorRescore.id() {
            payload_offset = Some(u64::from_le_bytes(
                bytes[entry + 8..entry + 16]
                    .try_into()
                    .expect("region offset"),
            ) as usize);
            break;
        }
    }
    let offset = payload_offset.expect("rescore region") + 32;
    bytes[offset] ^= 0x01;
    write(&segment, &bytes);
    let segment_id = segment_id(&segment);
    let controller = storage_controller(
        StorageTestFault::CorruptSegmentRegion,
        StorageFaultPlan::new(
            0,
            segment
                .file_name()
                .and_then(|name| name.to_str())
                .expect("segment artifact"),
        )
        .with_offset(u64::try_from(offset).expect("region offset fits u64"))
        .with_segment_region(segment_id, RegionKind::VectorRescore.id(), 0),
    );
    if let Ok(store) = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    ) {
        let error = store
            .search(
                SearchRequest::new(&[1.0, 0.0]),
                1,
                SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect_err("corrupt rescore region must return no candidates");
        let zeppelin_embed::lifecycle::QueryError::Store(
            zeppelin_embed::lifecycle::StoreError::Segment(
                zeppelin_embed::segment::SegmentError::Format(format),
            ),
        ) = error
        else {
            panic!("wrong corrupt-region query error: {error:?}");
        };
        assert_eq!(
            format.check(),
            zeppelin_embed::format::frame::FormatCheck::BlockChecksum
        );
        assert!(matches!(
            format.values(),
            zeppelin_embed::format::frame::FormatValues::Checksum { expected, actual }
                if expected != actual
        ));
    }
    let receipt = assert_receipt(
        &controller,
        "format-check",
        "corrupt-segment-region",
        "SegmentRead.RegionChecksum",
    );
    assert_eq!(receipt.plan().segment(), Some(segment_id));
    assert_eq!(
        (receipt.plan().region_kind(), receipt.plan().chunk()),
        (Some(RegionKind::VectorRescore.id()), Some(0))
    );
    let StorageReceiptObserved::SegmentChecksum {
        artifact,
        segment,
        region_kind,
        chunk,
        expected_checksum,
        actual_checksum,
    } = receipt.observed()
    else {
        panic!("segment checksum receipt had wrong typed effect");
    };
    assert_eq!(artifact, &segment_id.file_name());
    assert_eq!(*segment, segment_id);
    assert_eq!((*region_kind, *chunk), (RegionKind::VectorRescore.id(), 0));
    assert_ne!(expected_checksum, actual_checksum);
}

#[test]
fn storage_wrong_manifest_object_can_fire() {
    let directory = sealed_fixture();
    write(
        &directory.path().join("manifest.ze"),
        &read(&segment_path(directory.path())),
    );
    let controller = storage_controller(
        StorageTestFault::WrongManifestObject,
        StorageFaultPlan::new(0, "manifest.ze").with_offset(8),
    );
    let error = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .err()
    .expect("segment bytes at manifest path must refuse open");
    let zeppelin_embed::lifecycle::StoreError::Manifest(
        zeppelin_embed::manifest::ManifestError::Format(format),
    ) = error
    else {
        panic!("wrong public manifest error: {error:?}");
    };
    assert_eq!(
        format.values(),
        &zeppelin_embed::format::frame::FormatValues::Family {
            expected: zeppelin_embed::format::FormatFamily::Manifest.id(),
            actual: zeppelin_embed::format::FormatFamily::Segment.id(),
        }
    );
    let receipt = assert_receipt(
        &controller,
        "format-check",
        "wrong-manifest-object",
        "ManifestOpen.FamilyValidation",
    );
    assert_eq!(receipt.plan().offset(), Some(8));
    assert_eq!(
        receipt.observed(),
        &StorageReceiptObserved::Format {
            artifact: "manifest.ze".to_owned(),
            check: zeppelin_embed::format::frame::FormatCheck::Family,
            expected_family: Some(zeppelin_embed::format::FormatFamily::Manifest.id()),
            actual_family: Some(zeppelin_embed::format::FormatFamily::Segment.id()),
            expected_id: None,
            actual_id: None,
        }
    );
}

#[test]
fn storage_wrong_segment_object_can_fire() {
    let directory = sealed_fixture();
    let segment = segment_path(directory.path());
    write(&segment, &read(&directory.path().join("manifest.ze")));
    let controller = storage_controller(
        StorageTestFault::WrongSegmentObject,
        StorageFaultPlan::new(
            0,
            segment
                .file_name()
                .and_then(|name| name.to_str())
                .expect("segment artifact"),
        )
        .with_offset(8),
    );
    let error = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .err()
    .expect("manifest bytes at segment path must refuse open");
    assert!(matches!(
        error,
        zeppelin_embed::lifecycle::StoreError::Segment(
            zeppelin_embed::segment::SegmentError::Format(ref format)
        ) if format.check() == zeppelin_embed::format::frame::FormatCheck::Family
    ));
    let receipt = assert_receipt(
        &controller,
        "format-check",
        "wrong-segment-object",
        "SegmentOpen.FamilyValidation",
    );
    assert_eq!(receipt.plan().offset(), Some(8));
    assert_eq!(
        receipt.observed(),
        &StorageReceiptObserved::Format {
            artifact: segment
                .file_name()
                .and_then(|name| name.to_str())
                .expect("segment artifact")
                .to_owned(),
            check: zeppelin_embed::format::frame::FormatCheck::Family,
            expected_family: Some(zeppelin_embed::format::FormatFamily::Segment.id()),
            actual_family: Some(zeppelin_embed::format::FormatFamily::Manifest.id()),
            expected_id: Some(segment_id(&segment)),
            actual_id: None,
        }
    );
}

#[test]
fn storage_wrong_segment_identity_can_fire() {
    let directory = tempdir().expect("wrong identity directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open fixture");
    ingest_one(&store, 96);
    store.seal().expect("seal first segment");
    ingest_one(&store, 97);
    store.seal().expect("seal second segment");
    store.close().expect("close two-segment fixture");
    let mut segments = std::fs::read_dir(directory.path())
        .expect("list segments")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        })
        .collect::<Vec<_>>();
    segments.sort_unstable();
    assert_eq!(segments.len(), 2);
    write(&segments[0], &read(&segments[1]));
    let controller = storage_controller(
        StorageTestFault::WrongSegmentObject,
        StorageFaultPlan::new(
            0,
            segments[0]
                .file_name()
                .and_then(|name| name.to_str())
                .expect("segment artifact"),
        ),
    );
    let error = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .err()
    .expect("same-family wrong object must refuse open");
    assert!(matches!(
        error,
        zeppelin_embed::lifecycle::StoreError::Segment(
            zeppelin_embed::segment::SegmentError::WrongObject { .. }
        )
    ));
    let receipt = assert_receipt(
        &controller,
        "format-check",
        "wrong-segment-object",
        "SegmentOpen.ObjectIdentity",
    );
    let StorageReceiptObserved::Format {
        artifact,
        check,
        expected_family,
        actual_family,
        expected_id,
        actual_id,
    } = receipt.observed()
    else {
        panic!("wrong-object receipt had wrong typed effect");
    };
    assert_eq!(artifact, receipt.plan().artifact());
    assert_eq!(
        *check,
        zeppelin_embed::format::frame::FormatCheck::ObjectIdentity
    );
    assert_eq!((*expected_family, *actual_family), (None, None));
    assert_eq!(*expected_id, Some(segment_id(&segments[0])));
    assert_eq!(*actual_id, Some(segment_id(&segments[1])));
}

fn omission_fixture(file_name: &str) -> tempfile::TempDir {
    let directory = sealed_fixture();
    write(&directory.path().join(file_name), b"eligible-orphan");
    directory
}

#[test]
fn storage_list_omission_can_fire() {
    for file_name in [
        "segment-cccccccccccccccccccccccccccccccc.zseg",
        ".segment-cccccccccccccccccccccccccccccccc.zseg.tmp",
        ".manifest.ze.tmp",
    ] {
        let directory = omission_fixture(file_name);
        let controller = storage_controller(
            StorageTestFault::ListOmission {
                file_name: file_name.to_owned(),
            },
            StorageFaultPlan::new(0, file_name),
        );
        let faulted = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::default(),
            dependencies(controller.clone()),
        )
        .expect("list omission open");
        let receipt = assert_receipt(
            &controller,
            "orphan-cleanup",
            "list-delete-omission",
            "OrphanCleanup.List",
        );
        assert_eq!(
            receipt.observed(),
            &StorageReceiptObserved::Omission {
                artifact: file_name.to_owned(),
                deletion_observed: false,
            }
        );
        faulted.close().expect("close list omission leg");
        assert!(directory.path().join(file_name).exists());
        Store::open(directory.path(), OpenOptions::default())
            .expect("list omission recovery open")
            .close()
            .expect("close list recovery");
        assert!(!directory.path().join(file_name).exists());
    }
}

#[test]
fn storage_delete_omission_can_fire() {
    for file_name in [
        "segment-dddddddddddddddddddddddddddddddd.zseg",
        ".segment-dddddddddddddddddddddddddddddddd.zseg.tmp",
        ".manifest.ze.tmp",
    ] {
        let directory = omission_fixture(file_name);
        let controller = storage_controller(
            StorageTestFault::DeleteOmission {
                file_name: file_name.to_owned(),
            },
            StorageFaultPlan::new(0, file_name),
        );
        let faulted = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::default(),
            dependencies(controller.clone()),
        )
        .expect("delete omission open");
        let receipt = assert_receipt(
            &controller,
            "orphan-cleanup",
            "list-delete-omission",
            "OrphanCleanup.Delete",
        );
        assert_eq!(
            receipt.observed(),
            &StorageReceiptObserved::Omission {
                artifact: file_name.to_owned(),
                deletion_observed: false,
            }
        );
        faulted.close().expect("close delete omission leg");
        assert!(directory.path().join(file_name).exists());
        Store::open(directory.path(), OpenOptions::default())
            .expect("delete omission recovery open")
            .close()
            .expect("close delete recovery");
        assert!(!directory.path().join(file_name).exists());
    }
}

#[test]
fn draining_a_storage_receipt_does_not_rearm_the_fault() {
    let file_name = "segment-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.zseg";
    let directory = omission_fixture(file_name);
    let controller = storage_controller(
        StorageTestFault::ListOmission {
            file_name: file_name.to_owned(),
        },
        StorageFaultPlan::new(0, file_name),
    );
    Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .expect("first omission leg opens")
    .close()
    .expect("close first omission leg");
    assert_receipt(
        &controller,
        "orphan-cleanup",
        "list-delete-omission",
        "OrphanCleanup.List",
    );
    assert!(directory.path().join(file_name).exists());

    Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .expect("drained controller stays disarmed")
    .close()
    .expect("close disarmed recovery leg");
    assert!(
        !directory.path().join(file_name).exists(),
        "taking the first receipt rearmed the one-shot omission"
    );
    assert!(
        controller.take_receipt().is_none(),
        "one-shot controller emitted a second receipt"
    );
}

#[test]
fn delete_omission_cleanup_report_is_truthful() {
    let file_name = "segment-ffffffffffffffffffffffffffffffff.zseg";
    let directory = omission_fixture(file_name);
    let controller = storage_controller(
        StorageTestFault::DeleteOmission {
            file_name: file_name.to_owned(),
        },
        StorageFaultPlan::new(0, file_name),
    );
    Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .expect("delete omission opens")
    .close()
    .expect("close delete omission");
    let report = controller
        .take_cleanup_report()
        .expect("cleanup report from public Store open");
    assert_eq!(report.reclaimed_bytes(), 0);
    assert!(report.deleted_paths().is_empty());
    assert_eq!(report.retained_eligible_paths(), &[file_name.to_owned()]);
    assert!(!report.directory_synced());
}

fn corrupt_rescore_region(directory: &Path) {
    let segment = segment_path(directory);
    let mut bytes = read(&segment);
    let region_count = u16::from_le_bytes([bytes[52], bytes[53]]) as usize;
    let offset = (0..region_count)
        .find_map(|index| {
            let entry = 64 + index * REGION_ENTRY_LEN;
            (u16::from_le_bytes([bytes[entry], bytes[entry + 1]]) == RegionKind::VectorRescore.id())
                .then(|| {
                    u64::from_le_bytes(
                        bytes[entry + 8..entry + 16]
                            .try_into()
                            .expect("rescore region offset"),
                    ) as usize
                })
        })
        .expect("rescore region");
    bytes[offset + 32] ^= 1;
    write(&segment, &bytes);
}

#[test]
fn segment_checksum_receipts_are_scoped_to_one_store() {
    let store_a_directory = sealed_fixture();
    let store_b_directory = sealed_fixture();
    assert_eq!(
        segment_path(store_a_directory.path())
            .file_name()
            .expect("store A segment name"),
        segment_path(store_b_directory.path())
            .file_name()
            .expect("store B segment name"),
        "fixture must exercise identical segment IDs across stores"
    );
    corrupt_rescore_region(store_b_directory.path());
    let store_a_segment = segment_path(store_a_directory.path());
    let store_b_segment = segment_path(store_b_directory.path());

    let controller_a = storage_controller(
        StorageTestFault::CorruptSegmentRegion,
        StorageFaultPlan::new(
            0,
            store_a_segment
                .file_name()
                .and_then(|name| name.to_str())
                .expect("store A segment artifact"),
        )
        .with_segment_region(
            segment_id(&store_a_segment),
            RegionKind::VectorRescore.id(),
            0,
        ),
    );
    let store_a = Store::open_with_test_dependencies(
        store_a_directory.path(),
        OpenOptions::default(),
        dependencies(controller_a.clone()),
    )
    .expect("open clean store A");
    let controller_b = storage_controller(
        StorageTestFault::CorruptSegmentRegion,
        StorageFaultPlan::new(
            0,
            store_b_segment
                .file_name()
                .and_then(|name| name.to_str())
                .expect("store B segment artifact"),
        )
        .with_segment_region(
            segment_id(&store_b_segment),
            RegionKind::VectorRescore.id(),
            0,
        ),
    );
    let store_b = Store::open_with_test_dependencies(
        store_b_directory.path(),
        OpenOptions::default(),
        dependencies(controller_b.clone()),
    )
    .expect("open corrupt store B lazily");
    let _error = store_b
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            1,
            SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect_err("store B checksum corruption must refuse query");
    assert!(
        controller_a.take_receipt().is_none(),
        "store B validation emitted a receipt into store A's controller"
    );
    assert_receipt(
        &controller_b,
        "format-check",
        "corrupt-segment-region",
        "SegmentRead.RegionChecksum",
    );
    store_b.close().expect("close store B");
    store_a.close().expect("close store A");
}

#[test]
fn same_seed_clean_fault_and_retry_converge_exactly() {
    let clean_directory = tempdir().expect("clean control directory");
    let fault_directory = tempdir().expect("fault pair directory");
    let document = || {
        IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(0x5eed), Revision::new(7)),
            vec![0.25, -0.5],
        )])
    };

    let clean = Store::open(clean_directory.path(), OpenOptions::default()).expect("open clean");
    let clean_result = clean.ingest(document()).expect("clean durable mutation");
    clean.close().expect("close clean control");
    let clean_wal = read(&clean_directory.path().join("wal.ze"));

    let controller = storage_controller(
        StorageTestFault::PostCommitError,
        StorageFaultPlan::new(0, "wal.ze"),
    );
    let faulted = Store::open_with_test_dependencies(
        fault_directory.path(),
        OpenOptions::default(),
        dependencies(controller.clone()),
    )
    .expect("open fault pair");
    faulted
        .ingest(document())
        .expect_err("fault caller observes append ambiguity");
    assert_receipt(
        &controller,
        "retry",
        "post-commit-error",
        "WalCommit.AppendAfterInnerSuccess",
    );
    faulted.close().expect("close ambiguous fault leg");
    let retry = Store::open(fault_directory.path(), OpenOptions::default())
        .expect("reopen fault pair for retry");
    let retry_result = retry.ingest(document()).expect("exact retry");
    retry.close().expect("close retry leg");
    let retry_wal = read(&fault_directory.path().join("wal.ze"));

    assert_eq!(retry_result.seq(), clean_result.seq());
    assert_eq!(retry_result.generation(), clean_result.generation());
    assert_eq!(retry_wal, clean_wal, "same-seed fault/retry WAL drifted");
    let clean = Store::open(clean_directory.path(), OpenOptions::default()).expect("reopen clean");
    let retried =
        Store::open(fault_directory.path(), OpenOptions::default()).expect("reopen retry");
    assert_eq!(clean.stats().expect("clean stats").active_row_count, 1);
    assert_eq!(retried.stats().expect("retry stats").active_row_count, 1);
    clean.close().expect("close reopened clean");
    retried.close().expect("close reopened retry");
}

#[test]
fn store_open_cleans_only_eligible_orphans() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("create store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(1), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("ingest fixture row");
    store.seal().expect("seal fixture row");
    let reachable_name = store.snapshot().expect("snapshot").segments()[0]
        .meta()
        .id
        .file_name();
    store.close().expect("close fixture store");

    let reachable = directory.path().join(reachable_name);
    let writer_lock = directory.path().join("writer.lock");
    let wrong_writer_lock = directory.path().join(".writer.lock");
    assert!(
        writer_lock.exists(),
        "canonical writer.lock control file missing"
    );
    assert!(
        !wrong_writer_lock.exists(),
        "obsolete .writer.lock name appeared"
    );
    let reachable_bytes = read(&reachable);
    let orphan_final = directory
        .path()
        .join("segment-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.zseg");
    let orphan_segment_temp = directory
        .path()
        .join(".segment-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.zseg.tmp");
    let orphan_manifest_temp = directory.path().join(".manifest.ze.tmp");
    let purge_temp = directory.path().join(".purge.ze.tmp");
    let purge_wal_temp = directory.path().join(".wal.ze.purge.tmp");
    let unknown = directory.path().join("owner-sentinel.bin");
    write(&orphan_final, b"eligible-final");
    write(&orphan_segment_temp, b"eligible-segment-temp");
    write(&orphan_manifest_temp, b"eligible-manifest-temp");
    write(&purge_temp, b"preserved-purge-temp");
    write(&purge_wal_temp, b"preserved-purge-wal-temp");
    write(&unknown, b"preserved-unknown");

    let read_only = Store::open(directory.path(), OpenOptions::read_only())
        .expect("read-only open must not mutate");
    read_only.close().expect("close read-only store");
    assert_eq!(read(&reachable), reachable_bytes);
    assert_eq!(read(&orphan_final), b"eligible-final");
    assert_eq!(read(&orphan_segment_temp), b"eligible-segment-temp");
    assert_eq!(read(&orphan_manifest_temp), b"eligible-manifest-temp");
    assert_eq!(read(&purge_temp), b"preserved-purge-temp");
    assert_eq!(read(&purge_wal_temp), b"preserved-purge-wal-temp");
    assert_eq!(read(&unknown), b"preserved-unknown");
    assert!(writer_lock.exists(), "read-only open removed writer.lock");

    let vfs = Arc::new(CountingVfs::new(StdVfs));
    let dependencies = StoreTestDependencies::new(
        Arc::clone(&vfs) as Arc<dyn zeppelin_embed::vfs::Vfs>,
        Arc::new(SystemMonotonicClock),
    );
    let read_write = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable),
        dependencies,
    )
    .expect("read-write open cleans eligible orphans");

    assert_eq!(read(&reachable), reachable_bytes);
    assert!(!orphan_final.exists(), "eligible final orphan remained");
    assert!(
        !orphan_segment_temp.exists(),
        "eligible segment temp orphan remained"
    );
    assert!(
        !orphan_manifest_temp.exists(),
        "eligible manifest temp orphan remained"
    );
    assert_eq!(read(&purge_temp), b"preserved-purge-temp");
    assert_eq!(read(&purge_wal_temp), b"preserved-purge-wal-temp");
    assert_eq!(read(&unknown), b"preserved-unknown");
    assert!(
        writer_lock.exists(),
        "read-write cleanup removed writer.lock"
    );
    assert_eq!(vfs.delete_calls(), 3, "exact eligible deletion count");
    assert_eq!(
        vfs.full_sync_calls(),
        2,
        "one directory sync for manifest adoption and one after cleanup"
    );
    read_write.close().expect("close read-write store");
}

#[test]
fn read_only_open_preserves_a_committed_purge_intent_and_all_store_bytes() {
    let directory = sealed_fixture();
    let store =
        Store::open(directory.path(), OpenOptions::default()).expect("reopen purge fixture");
    let token = store
        .purge_with_available_space(&[DocId::new(92)], u64::MAX)
        .expect("commit purge intent");
    assert!(!token.is_no_op());
    store.close().expect("close with committed purge intent");
    assert!(directory.path().join("purge.ze").exists());
    let before = directory_bytes(directory.path());

    let error = Store::open(directory.path(), OpenOptions::read_only())
        .err()
        .expect("read-only open cannot execute committed purge recovery");
    assert!(matches!(
        error,
        zeppelin_embed::lifecycle::StoreError::PurgeRecovery { ref detail }
            if detail == "store handle is read-only"
    ));
    assert_eq!(directory_bytes(directory.path()), before);
    assert!(directory.path().join("purge.ze").exists());

    Store::open(directory.path(), OpenOptions::default())
        .expect("read-write open recovers committed purge")
        .close()
        .expect("close recovered purge fixture");
    assert!(!directory.path().join("purge.ze").exists());
}
