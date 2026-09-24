//! End-to-end store verification against deliberately damaged stores.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use tempfile::{TempDir, tempdir};
use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::format::frame::{FILE_HEADER_LEN, FILE_TRAILER_LEN};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::manifest::io::MANIFEST_FILE;
use zeppelin_embed::manifest::{decode_manifest, encode_manifest};
use zeppelin_embed::meta::{ColumnDefinition, ColumnId, ColumnType, PredicateValue, Schema};
use zeppelin_embed::segment::layout::{REGION_ENTRY_LEN, RegionKind, SEGMENT_PREFIX_LEN};
use zeppelin_embed::verify::{FindingKind, VerifyError, verify_store};
use zeppelin_embed::wal::LogSeq;
use zeppelin_embed::wal::header::encode_header;

const WAL: &str = "wal.ze";
const WAL_HEADER_LEN: usize = 40;
const FILTER: ColumnId = ColumnId::new(1);
const DIRECTORY_START: usize = FILE_HEADER_LEN + SEGMENT_PREFIX_LEN;

fn schema() -> Schema {
    Schema::new(vec![ColumnDefinition::new(
        FILTER,
        "filter",
        ColumnType::U64,
        true,
    )])
    .expect("schema")
}

fn document(id: u128, revision: u64) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vec![id as f32, 1.0],
    )
    .with_text(format!("meeting note {id} about the harbour"))
    .with_columns(vec![(FILTER, PredicateValue::U64(id as u64))])
}

/// A closed store with one sealed segment (columns, alive, postings, stored
/// text) and an unabsorbed WAL tail holding upserts and a delete.
fn fixture() -> TempDir {
    let directory = tempdir().expect("store directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_schema(schema()),
    )
    .expect("open store");
    store
        .ingest(IngestBatch::new(
            (1..=8).map(|id| document(id, 1)).collect(),
        ))
        .expect("ingest sealed rows");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![document(9, 1), document(2, 2)]))
        .expect("ingest WAL tail");
    store
        .delete(DeleteBatch::new(vec![DocId::new(9)]))
        .expect("delete tail row");
    store.close().expect("close");
    directory
}

fn file_hashes(directory: &Path) -> BTreeMap<String, (u64, u64)> {
    fs::read_dir(directory)
        .expect("list store")
        .map(|entry| {
            let entry = entry.expect("entry");
            let bytes = fs::read(entry.path()).expect("read store file");
            (
                entry.file_name().to_string_lossy().into_owned(),
                (bytes.len() as u64, xxh3_64(&bytes)),
            )
        })
        .collect()
}

fn segment_file(directory: &Path) -> String {
    fs::read_dir(directory)
        .expect("list store")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .find(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        .expect("sealed segment")
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// Directory index, offset and length of the first region of `kind`.
fn region(bytes: &[u8], kind: RegionKind) -> (usize, usize, usize) {
    let count = u16::from_le_bytes(
        bytes[FILE_HEADER_LEN + 20..FILE_HEADER_LEN + 22]
            .try_into()
            .unwrap(),
    ) as usize;
    (0..count)
        .map(|index| {
            let entry = DIRECTORY_START + index * REGION_ENTRY_LEN;
            let id = u16::from_le_bytes(bytes[entry..entry + 2].try_into().unwrap());
            (index, id, entry)
        })
        .find(|(_, id, _)| *id == kind.id())
        .map(|(index, _, entry)| {
            (
                index,
                read_u64(bytes, entry + 8) as usize,
                read_u64(bytes, entry + 16) as usize,
            )
        })
        .unwrap_or_else(|| panic!("segment has no {kind:?} region"))
}

/// Rewrites one region checksum plus the header and file checksums, so the
/// damaged bytes are checksum-valid and only a decoder can reject them.
fn restamp_region(bytes: &mut [u8], index: usize) {
    let entry = DIRECTORY_START + index * REGION_ENTRY_LEN;
    let offset = read_u64(bytes, entry + 8) as usize;
    let length = read_u64(bytes, entry + 16) as usize;
    let checksum = xxh3_64(&bytes[offset..offset + length]).to_le_bytes();
    bytes[entry + 24..entry + 32].copy_from_slice(&checksum);
    let header_length = read_u64(bytes, 16) as usize;
    let header_checksum = xxh3_64(&bytes[..header_length - 8]).to_le_bytes();
    bytes[header_length - 8..header_length].copy_from_slice(&header_checksum);
    let trailer = bytes.len() - FILE_TRAILER_LEN;
    let file_checksum = xxh3_64(&bytes[..trailer]).to_le_bytes();
    bytes[trailer..].copy_from_slice(&file_checksum);
}

fn flip(path: &Path, offset: usize) {
    let mut bytes = fs::read(path).expect("read");
    bytes[offset] ^= 0x5a;
    fs::write(path, bytes).expect("write");
}

fn truncate_by(path: &Path, removed: usize) {
    let bytes = fs::read(path).expect("read");
    fs::write(path, &bytes[..bytes.len() - removed]).expect("write");
}

#[test]
fn verify_reports_a_clean_store_and_leaves_every_byte_unchanged() {
    let directory = fixture();
    let before = file_hashes(directory.path());
    let report = verify_store(directory.path()).expect("verify");
    assert_eq!(report.findings, Vec::new());
    assert!(report.is_clean());
    assert_eq!(report.segments_checked, 1);
    assert!(report.generation > 0);
    assert_eq!(
        report.wal_records_checked, 3,
        "the seal truncates the WAL (ZE-233); the tail holds one record \
         per document and one delete"
    );
    assert_eq!(file_hashes(directory.path()), before);
    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen");
    reopened.close().expect("close");

    let empty = tempdir().expect("empty store");
    let report = verify_store(empty.path()).expect("verify empty");
    assert!(report.is_clean());
    assert_eq!(report.generation, 0);
    assert_eq!(fs::read_dir(empty.path()).expect("list").count(), 0);
}

#[test]
fn verify_rejects_a_path_that_is_not_a_store_directory() {
    let directory = tempdir().expect("directory");
    let missing = directory.path().join("absent");
    assert!(matches!(
        verify_store(&missing),
        Err(VerifyError::NotFound { .. })
    ));
    assert!(!missing.exists());
    let file = directory.path().join("file");
    fs::write(&file, b"not a store").expect("write file");
    assert!(matches!(
        verify_store(&file),
        Err(VerifyError::NotDirectory { .. })
    ));
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    ManifestFlip,
    ManifestTruncate,
    ManifestDelete,
    ManifestDisagreesWithSegment,
    SegmentHeaderFlip,
    SegmentPostingsFlip,
    SegmentTruncate,
    SegmentDelete,
    SegmentAliveInconsistent,
    WalHeaderFlip,
    WalRecordFlip,
    WalTornTail,
    WalHeaderOnly,
    WalDelete,
}

fn damage(directory: &Path, segment: &str, damage: Damage) {
    let manifest = directory.join(MANIFEST_FILE);
    let segment = directory.join(segment);
    let wal = directory.join(WAL);
    match damage {
        Damage::ManifestFlip => flip(&manifest, 60),
        Damage::ManifestTruncate => truncate_by(&manifest, 9),
        Damage::ManifestDelete => fs::remove_file(&manifest).expect("delete manifest"),
        Damage::ManifestDisagreesWithSegment => {
            let mut decoded =
                decode_manifest("manifest", &fs::read(&manifest).expect("read")).expect("decode");
            decoded.segments[0].row_count += 1;
            fs::write(&manifest, encode_manifest(&decoded).expect("encode")).expect("write");
        }
        Damage::SegmentHeaderFlip => flip(&segment, FILE_HEADER_LEN + 28),
        Damage::SegmentPostingsFlip => {
            let bytes = fs::read(&segment).expect("read");
            let (_, offset, length) = region(&bytes, RegionKind::Postings);
            flip(&segment, offset + length / 2);
        }
        Damage::SegmentTruncate => truncate_by(&segment, 100),
        Damage::SegmentDelete => fs::remove_file(&segment).expect("delete segment"),
        Damage::SegmentAliveInconsistent => {
            let mut bytes = fs::read(&segment).expect("read");
            let (index, offset, _) = region(&bytes, RegionKind::Alive);
            // The declared bitmap length no longer matches the row count.
            bytes[offset + 4] = bytes[offset + 4].wrapping_add(1);
            restamp_region(&mut bytes, index);
            fs::write(&segment, bytes).expect("write");
        }
        Damage::WalHeaderFlip => flip(&wal, 4),
        Damage::WalRecordFlip => flip(&wal, WAL_HEADER_LEN + 20),
        Damage::WalTornTail => truncate_by(&wal, 3),
        Damage::WalHeaderOnly => {
            // A header-only WAL at the seal's boundary is the normal state
            // after a seal (ZE-233), so start the log before the manifest's
            // absorbed sequence: the manifest is then ahead of the WAL.
            let header = encode_header(LogSeq::new(1)).expect("WAL header");
            assert_eq!(header.len(), WAL_HEADER_LEN);
            fs::write(&wal, header).expect("write");
        }
        Damage::WalDelete => fs::remove_file(&wal).expect("delete WAL"),
    }
}

#[test]
fn verify_names_each_corruption_with_a_specific_finding() {
    let cases = [
        (
            Damage::ManifestFlip,
            FindingKind::ManifestCorrupt,
            "manifest",
        ),
        (
            Damage::ManifestTruncate,
            FindingKind::ManifestCorrupt,
            "manifest",
        ),
        (
            Damage::ManifestDelete,
            FindingKind::ManifestMissing,
            "manifest",
        ),
        (
            Damage::ManifestDisagreesWithSegment,
            FindingKind::SegmentMismatch,
            "segment",
        ),
        (
            Damage::SegmentHeaderFlip,
            FindingKind::SegmentCorrupt,
            "segment",
        ),
        (
            Damage::SegmentPostingsFlip,
            FindingKind::SegmentRegionCorrupt,
            "segment",
        ),
        (
            Damage::SegmentTruncate,
            FindingKind::SegmentCorrupt,
            "segment",
        ),
        (
            Damage::SegmentDelete,
            FindingKind::SegmentMissing,
            "segment",
        ),
        (
            Damage::SegmentAliveInconsistent,
            FindingKind::SegmentIndexInvalid,
            "segment",
        ),
        (Damage::WalHeaderFlip, FindingKind::WalHeaderCorrupt, "wal"),
        (Damage::WalRecordFlip, FindingKind::WalRecordCorrupt, "wal"),
        (Damage::WalTornTail, FindingKind::WalRecordCorrupt, "wal"),
        (
            Damage::WalHeaderOnly,
            FindingKind::ManifestAheadOfWal,
            "manifest",
        ),
        (Damage::WalDelete, FindingKind::WalMissing, "wal"),
    ];
    for (case, kind, file) in cases {
        let directory = fixture();
        let segment = segment_file(directory.path());
        damage(directory.path(), &segment, case);
        let before = file_hashes(directory.path());
        let report = verify_store(directory.path())
            .unwrap_or_else(|error| panic!("{case:?}: verify failed to run: {error}"));
        assert_eq!(
            report.findings.len(),
            1,
            "{case:?}: exactly one finding, got {:#?}",
            report.findings
        );
        let finding = &report.findings[0];
        assert_eq!(finding.kind, kind, "{case:?}: {finding:#?}");
        let expected_file = match file {
            "manifest" => MANIFEST_FILE.to_owned(),
            "segment" => segment.clone(),
            _ => WAL.to_owned(),
        };
        assert_eq!(finding.file, expected_file, "{case:?}");
        assert!(!finding.detail.is_empty(), "{case:?}");
        assert_eq!(
            file_hashes(directory.path()),
            before,
            "{case:?}: verify modified the store"
        );
        match case {
            Damage::SegmentPostingsFlip => {
                let bytes = fs::read(directory.path().join(&segment)).expect("read");
                let (_, offset, _) = region(&bytes, RegionKind::Postings);
                assert_eq!(finding.offset, Some(offset as u64));
                assert!(finding.detail.contains("Postings"), "{finding:#?}");
            }
            Damage::WalRecordFlip => assert_eq!(finding.offset, Some(WAL_HEADER_LEN as u64)),
            Damage::WalTornTail => {
                let length = fs::read(directory.path().join(WAL)).expect("read").len() as u64;
                let offset = finding.offset.expect("torn tail offset");
                assert!(
                    offset > WAL_HEADER_LEN as u64 && offset < length,
                    "{finding:#?}"
                );
            }
            Damage::WalHeaderFlip => assert_eq!(finding.offset, Some(0)),
            _ => {}
        }
        // The damage is real: open refuses it, except where open reads only
        // headers and the region is checked lazily at query time.
        let lazily_checked = matches!(case, Damage::SegmentPostingsFlip);
        assert_eq!(
            Store::open(directory.path(), OpenOptions::read_only()).is_err(),
            !lazily_checked,
            "{case:?}: open disagrees with verify about this damage"
        );
    }
}

#[test]
fn verify_keeps_walking_after_the_first_damaged_file() {
    let directory = fixture();
    let segment = segment_file(directory.path());
    damage(directory.path(), &segment, Damage::SegmentDelete);
    damage(directory.path(), &segment, Damage::WalRecordFlip);
    let report = verify_store(directory.path()).expect("verify");
    let found = report
        .findings
        .iter()
        .map(|finding| (finding.kind, finding.file.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        vec![
            (FindingKind::SegmentMissing, segment),
            (FindingKind::WalRecordCorrupt, WAL.to_owned()),
        ]
    );
}

#[test]
fn verify_accepts_orphans_that_open_removes_without_data_loss() {
    let directory = fixture();
    // A segment written by an interrupted seal that never reached the
    // manifest is an orphan, not damage.
    let orphan = directory
        .path()
        .join("segment-00000000000000000000000000000001.zseg");
    fs::write(&orphan, b"interrupted seal").expect("write orphan");
    fs::write(directory.path().join(".manifest.ze.tmp"), b"torn temp").expect("write temp");
    let report = verify_store(directory.path()).expect("verify");
    assert_eq!(report.findings, Vec::new());
}

#[test]
fn verify_decodes_segment_columns_against_the_manifest_schema() {
    let directory = fixture();
    // An added nullable attribute evolves the manifest schema; older
    // segments read it as null, so the store stays clean.
    let evolved = Schema::new(vec![
        ColumnDefinition::new(FILTER, "filter", ColumnType::U64, true),
        ColumnDefinition::new(ColumnId::new(2), "added", ColumnType::I64, true),
    ])
    .expect("evolved schema");
    Store::open(
        directory.path(),
        OpenOptions::default().with_schema(evolved),
    )
    .expect("evolve schema")
    .close()
    .expect("close");
    assert_eq!(verify_store(directory.path()).expect("verify").findings, []);

    // A manifest whose schema changes the persisted column's type conflicts
    // with the sealed segment, and open refuses it.
    let manifest = directory.path().join(MANIFEST_FILE);
    let mut decoded =
        decode_manifest("manifest", &fs::read(&manifest).expect("read")).expect("decode");
    decoded.schema = Schema::new(vec![ColumnDefinition::new(
        FILTER,
        "filter",
        ColumnType::I64,
        true,
    )])
    .expect("conflicting schema");
    fs::write(&manifest, encode_manifest(&decoded).expect("encode")).expect("write");
    let before = file_hashes(directory.path());
    let report = verify_store(directory.path()).expect("verify");
    let kinds = report
        .findings
        .iter()
        .map(|finding| finding.kind)
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&FindingKind::SegmentIndexInvalid),
        "{:#?}",
        report.findings
    );
    assert_eq!(file_hashes(directory.path()), before);
    assert!(Store::open(directory.path(), OpenOptions::read_only()).is_err());
}

#[test]
fn verify_decodes_the_pending_purge_intent() {
    let directory = fixture();
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    store.purge(&[DocId::new(3)]).expect("record purge intent");
    store.close().expect("close with the purge pending");
    let intent = directory.path().join("purge.ze");
    assert!(intent.exists(), "purge left no pending intent");
    // A pending intent is a purge the next writable open completes.
    assert_eq!(verify_store(directory.path()).expect("verify").findings, []);

    fs::write(&intent, b"not a purge intent").expect("damage intent");
    let before = file_hashes(directory.path());
    let report = verify_store(directory.path()).expect("verify");
    assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
    assert_eq!(report.findings[0].kind, FindingKind::PurgeIntentCorrupt);
    assert_eq!(report.findings[0].file, "purge.ze");
    assert_eq!(file_hashes(directory.path()), before);
    assert!(Store::open(directory.path(), OpenOptions::default()).is_err());
}
