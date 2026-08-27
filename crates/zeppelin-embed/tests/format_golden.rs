#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::format::frame::{
    FILE_HEADER_LEN, FILE_TRAILER_LEN, FormatCheck, decode_artifact, encode_artifact,
};
use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::format::{FormatFamily, FormatRegistry, RegistryError};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::manifest::{
    EpochMeta, Manifest, ManifestError, decode_manifest, encode_manifest,
};
use zeppelin_embed::meta::{
    AliveSet, ColumnDefinition, ColumnId, ColumnInput, ColumnStoreBuilder, ColumnType, ColumnValue,
    Schema,
};
use zeppelin_embed::quant::quantize_bit4;
use zeppelin_embed::segment::layout::{Int8Factors, RegionKind};
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, SegmentStoredMetadata,
    SegmentStoredText, encode_segment, write_segment_with_documents,
    write_segment_with_documents_and_metadata, write_segment_with_documents_and_text,
};
use zeppelin_embed::segment::{ClusteringKeyRange, SegmentId, SegmentMeta};
use zeppelin_embed::vfs::StdVfs;

fn fixture(text: &str) -> Vec<u8> {
    decode_hex(text).expect("fixture hex")
}

fn epoch_manifest() -> Manifest {
    let document = EmbeddingTower {
        model_id: "document-model".to_owned(),
        model_version: "2.1".to_owned(),
        weights_digest: vec![0x10, 0x20, 0x30],
        dims: 4,
        normalization: Normalization::L2,
        prompt_prefix: "search_document: ".to_owned(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CoreMl,
        compute_units: ComputeUnits::CpuAndNeuralEngine,
        os_build: Some("25A100".to_owned()),
    };
    let query = EmbeddingTower {
        model_id: "query-model".to_owned(),
        model_version: "1.4".to_owned(),
        weights_digest: vec![0xa0, 0xb0, 0xc0],
        dims: 4,
        normalization: Normalization::L2,
        prompt_prefix: "search_query: ".to_owned(),
        max_tokens: 128,
        runtime: EmbeddingRuntime::Mlx,
        compute_units: ComputeUnits::CpuAndGpu,
        os_build: None,
    };
    let declared = StoreEpoch {
        embedding: EmbeddingEpoch {
            document,
            query,
            alignment_digest: vec![0xde, 0xad, 0xbe, 0xef],
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    };
    let identity = declared.identity();
    Manifest {
        generation: 21,
        log_seq: 13,
        segments: vec![SegmentMeta {
            id: SegmentId::new(9, [0x5a; 10]),
            row_count: 3,
            scheme: 4,
            dims: 4,
            file_size: 4096,
            epoch_id: Some(identity.embedding),
            clustering_key_range: ClusteringKeyRange::Unstamped,
        }],
        epochs: vec![EpochMeta::from(&declared)],
        epoch_alias: Some(identity),
        schema: Schema::new(vec![ColumnDefinition::new(
            ColumnId::new(1),
            "title",
            ColumnType::RawString,
            true,
        )])
        .expect("schema"),
    }
}

#[test]
fn a_manifest_with_epoch_registry_alias_and_segment_epoch_tags_is_byte_exact() {
    let manifest = epoch_manifest();
    let encoded = encode_manifest(&manifest).expect("epoch manifest");
    assert_eq!(
        encoded,
        fixture(include_str!("fixtures/format/manifest_v2.hex"))
    );
    assert_eq!(
        decode_manifest("manifest-epoch.golden", &encoded).expect("epoch manifest decode"),
        manifest
    );
}

#[test]
fn family_10_v1_manifest_fixtures_are_explicitly_rejected() {
    for (artifact, bytes) in [
        (
            "manifest-v1.golden",
            fixture(include_str!("fixtures/format/manifest_v1.hex")),
        ),
        (
            "manifest-clustering-ranges-v1.golden",
            fixture(include_str!(
                "fixtures/format/manifest_clustering_ranges_v1.hex"
            )),
        ),
    ] {
        let error = decode_manifest(artifact, &bytes).expect_err("family 10 v1 must be rejected");
        let ManifestError::Format(error) = error else {
            panic!("old manifest returned a non-format error: {error}");
        };
        assert_eq!(error.check(), FormatCheck::Version);
        assert!(error.detail().contains("version 1"), "{error}");
    }
}

#[test]
fn a_published_alias_must_name_an_epoch_registry_entry() {
    let mut manifest = epoch_manifest();
    let mut other = manifest.epochs[0].embedding.clone();
    other.query.weights_digest.push(0x99);
    manifest.epoch_alias = Some(zeppelin_embed::epoch::EpochIdentity {
        embedding: zeppelin_embed::epoch::EpochId::of(&other),
        tokenizer: manifest.epochs[0].tokenizer,
    });

    let error = encode_manifest(&manifest).expect_err("unknown alias must fail");
    assert!(matches!(error, ManifestError::UnknownEpochAlias { .. }));
}

#[test]
fn every_segment_epoch_tag_must_name_an_epoch_registry_entry() {
    let mut manifest = epoch_manifest();
    let mut other = manifest.epochs[0].embedding.clone();
    other.document.weights_digest.push(0x88);
    manifest.segments[0].epoch_id = Some(zeppelin_embed::epoch::EpochId::of(&other));

    let error = encode_manifest(&manifest).expect_err("unknown segment epoch must fail");
    assert!(matches!(error, ManifestError::UnknownSegmentEpoch { .. }));
}

#[test]
fn format_frame_golden_is_byte_exact_and_semantically_exact() {
    let encoded = encode_artifact(FormatFamily::Frame, 0x1020_3040, b"golden");
    assert_eq!(
        encoded.len(),
        FILE_HEADER_LEN + 8 + b"golden".len() + 8 + FILE_TRAILER_LEN
    );
    assert_eq!(
        encoded,
        fixture(include_str!("fixtures/format/frame_v1.hex"))
    );
    let decoded = decode_artifact("frame.golden", FormatFamily::Frame, &encoded)
        .expect("golden frame decodes");
    assert_eq!(decoded.header.flags, 0x1020_3040);
    assert_eq!(decoded.payload, b"golden");
}

fn golden_segment(dims: u32, rows: u32, int8: bool) -> (Vec<u8>, SegmentId) {
    let schema = if rows == 0 {
        Schema::new(Vec::new()).expect("schema")
    } else {
        Schema::new(vec![
            ColumnDefinition::new(ColumnId::new(1), "u", ColumnType::U64, true),
            ColumnDefinition::new(ColumnId::new(2), "i", ColumnType::I64, true),
            ColumnDefinition::new(ColumnId::new(3), "f", ColumnType::F64, true),
            ColumnDefinition::new(ColumnId::new(4), "b", ColumnType::Bool, true),
            ColumnDefinition::new(ColumnId::new(5), "d", ColumnType::DictionaryString, true),
            ColumnDefinition::new(ColumnId::new(6), "r", ColumnType::RawString, true),
        ])
        .expect("schema")
    };
    let mut builder = ColumnStoreBuilder::new(schema);
    for row in 0..rows {
        builder
            .push_row(
                -7 + i64::from(row),
                &[
                    ColumnInput {
                        column: ColumnId::new(1),
                        value: ColumnValue::U64(9),
                    },
                    ColumnInput {
                        column: ColumnId::new(2),
                        value: ColumnValue::I64(-3),
                    },
                    ColumnInput {
                        column: ColumnId::new(3),
                        value: ColumnValue::F64(1.5),
                    },
                    ColumnInput {
                        column: ColumnId::new(4),
                        value: ColumnValue::Bool(true),
                    },
                    ColumnInput {
                        column: ColumnId::new(5),
                        value: ColumnValue::String("x"),
                    },
                    ColumnInput {
                        column: ColumnId::new(6),
                        value: ColumnValue::String("raw"),
                    },
                ],
            )
            .expect("row");
    }
    let columns = builder.finish().expect("columns");
    let alive = AliveSet::new(rows);
    let rescore = (0..rows as usize * dims as usize)
        .map(|index| index as f32 * 0.25 - 1.0)
        .collect::<Vec<_>>();
    let id = SegmentId::new(u64::from(dims), [rows as u8; 10]);
    let bytes = if int8 {
        let codes = vec![3_u8; rows as usize * dims as usize];
        let factors = vec![
            Int8Factors {
                scale: 0.25,
                offset: -1.0,
            };
            rows as usize
        ];
        encode_segment(SegmentBuild {
            id,
            scheme: 2,
            dims,
            codes: &codes,
            factors: SegmentFactors::Int8(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("Int8 segment")
    } else {
        let mut codes = Vec::new();
        let mut factors = Vec::new();
        for row in rescore.chunks_exact(dims as usize) {
            let mut encoded = vec![0_u8; (dims as usize).div_ceil(2)];
            factors.push(quantize_bit4(row, &mut encoded).expect("Bit4"));
            codes.extend_from_slice(&encoded);
        }
        encode_segment(SegmentBuild {
            id,
            scheme: 4,
            dims,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        })
        .expect("Bit4 segment")
    };
    (bytes, id)
}

#[test]
fn format_every_registered_family_and_edge_shape_matches_checked_in_golden() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manifest = epoch_manifest();
    let manifest_bytes = encode_manifest(&manifest).expect("manifest");
    assert_eq!(
        manifest_bytes,
        fixture(include_str!("fixtures/format/manifest_v2.hex"))
    );
    assert_eq!(
        decode_manifest("manifest.golden", &manifest_bytes).expect("manifest decode"),
        manifest
    );

    let cases = [
        (
            "ZERO",
            3,
            0,
            false,
            include_str!("fixtures/format/segment_zero_header_v1.hex"),
        ),
        (
            "ONE",
            3,
            1,
            false,
            include_str!("fixtures/format/segment_one_header_v1.hex"),
        ),
        ("TAIL65", 65, 1, false, ""),
        ("INT8", 3, 1, true, ""),
    ];
    for (label, dims, rows, int8, header_fixture) in cases {
        let (bytes, id) = golden_segment(dims, rows, int8);
        let path = directory.path().join(format!("{label}.zseg"));
        std::fs::write(&path, &bytes).expect("write");
        let reader = SegmentReader::open(&path, id).expect("open");
        reader.validate_all().expect("all bytes");
        if !header_fixture.is_empty() {
            assert_eq!(
                &bytes[..reader.header_length()],
                fixture(header_fixture),
                "{label} segment header"
            );
        }
        match label {
            "ZERO" => {
                assert_eq!(reader.meta().row_count, 0);
                assert_eq!(reader.columns().expect("columns").row_count(), 0);
                assert_eq!(reader.alive().expect("alive").row_count(), 0);
                assert_eq!(
                    reader.region(RegionKind::Columns).expect("columns"),
                    fixture(include_str!("fixtures/format/columns_zero_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::Alive).expect("alive"),
                    fixture(include_str!("fixtures/format/alive_zero_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::ChecksumTable).expect("checksums"),
                    fixture(include_str!("fixtures/format/checksum_table_zero_v1.hex"))
                );
            }
            "ONE" => {
                assert_eq!(reader.meta().row_count, 1);
                assert_eq!(
                    reader.region(RegionKind::Columns).expect("columns"),
                    fixture(include_str!("fixtures/format/columns_one_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::Alive).expect("alive"),
                    fixture(include_str!("fixtures/format/alive_one_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::VectorCodes).expect("codes"),
                    fixture(include_str!("fixtures/format/vector_codes_odd_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::VectorFactors).expect("factors"),
                    fixture(include_str!("fixtures/format/vector_factors_bit4_v1.hex"))
                );
                assert_eq!(
                    reader.region(RegionKind::VectorRescore).expect("rescore"),
                    fixture(include_str!("fixtures/format/vector_rescore_one_v1.hex"))
                );
                assert_eq!(reader.bit4_codes().expect("odd codes").last(), Some(&0x40));
            }
            "TAIL65" => {
                assert_eq!(reader.meta().dims, 65);
                assert_eq!(
                    reader.region(RegionKind::VectorCodes).expect("tail codes"),
                    fixture(include_str!("fixtures/format/vector_codes_tail65_v1.hex"))
                );
                assert_eq!(reader.bit4_codes().expect("tail codes").last(), Some(&0xf0));
            }
            "INT8" => {
                assert_eq!(std::mem::size_of::<Int8Factors>(), 8);
                assert_eq!(
                    reader
                        .region(RegionKind::VectorFactors)
                        .expect("Int8 factors"),
                    fixture(include_str!("fixtures/format/vector_factors_int8_v1.hex"))
                );
                assert_eq!(
                    reader.int8_factors().expect("Int8 factors"),
                    &[Int8Factors {
                        scale: 0.25,
                        offset: -1.0,
                    }]
                );
            }
            actual => panic!("unknown golden case {actual}"),
        }
    }
    assert_eq!(
        std::mem::size_of::<zeppelin_embed::quant::Bit4Factors>(),
        12
    );
    assert_eq!(
        fixture(include_str!("fixtures/format/postings_reserved_v1.hex")),
        Vec::<u8>::new()
    );
    assert_eq!(FormatRegistry::families().len(), 16);
    assert_eq!(FormatFamily::Wal.id(), 11);
    assert_eq!(FormatFamily::DocumentVersions.id(), 13);
    assert_eq!(FormatFamily::StoredMetadata.id(), 14);
    assert_eq!(FormatFamily::PurgeIntent.id(), 15);
    assert_eq!(FormatFamily::StoredText.id(), 16);
    assert_eq!(RegionKind::StoredText.id(), 14);
    assert_eq!(
        FormatRegistry::require(FormatFamily::Wal.id(), 1)
            .expect("WAL family")
            .family,
        FormatFamily::Wal
    );
    assert_eq!(
        FormatRegistry::require_scheme(3),
        Err(RegistryError::RetiredScheme(3))
    );
    assert_eq!(
        FormatRegistry::require_scheme(5),
        Err(RegistryError::RetiredScheme(5))
    );

    let (mut unknown, id) = golden_segment(3, 0, false);
    unknown[192..194].copy_from_slice(&65_000_u16.to_le_bytes());
    let header_checksum = xxhash_rust::xxh3::xxh3_64(&unknown[..256]).to_le_bytes();
    unknown[256..264].copy_from_slice(&header_checksum);
    let trailer = unknown.len() - 8;
    let file_checksum = xxhash_rust::xxh3::xxh3_64(&unknown[..trailer]).to_le_bytes();
    unknown[trailer..].copy_from_slice(&file_checksum);
    assert_eq!(
        &unknown[..264],
        fixture(include_str!("fixtures/format/unknown_kind_header_v1.hex"))
    );
    let unknown_path = directory.path().join("UNKNOWN.zseg");
    std::fs::write(&unknown_path, &unknown).expect("unknown write");
    let reader = SegmentReader::open(&unknown_path, id).expect("unknown kind is skippable");
    assert_eq!(reader.unknown_region_ids(), vec![65_000]);
    reader
        .validate_all()
        .expect("unknown region bytes validate");

    let schema = Schema::new(Vec::new()).expect("document-version schema");
    let mut columns = ColumnStoreBuilder::new(schema);
    columns.push_row(0, &[]).expect("document-version row");
    let columns = columns.finish().expect("document-version columns");
    let version = DocumentVersion::new(
        DocId::new(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff),
        Revision::new(0x0102_0304_0506_0708),
    );
    let document_id = SegmentId::new(9, [0x13; 10]);
    write_segment_with_documents(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id: document_id,
            scheme: 4,
            dims: 2,
            codes: &[0x88],
            factors: SegmentFactors::Bit4(&[zeppelin_embed::quant::Bit4Factors::from_persisted(
                1.0, 1.0, 1.0,
            )]),
            rescore: &[0.0, 0.0],
            columns: &columns,
            alive: &AliveSet::new(1),
        },
        SegmentDocumentVersions {
            doc_ids: &[version.doc_id()],
            revisions: &[version.revision()],
        },
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("document-version policy"),
    )
    .expect("write document-version segment");
    let document_path = directory.path().join(document_id.file_name());
    let document_reader =
        SegmentReader::open(&document_path, document_id).expect("open document-version segment");
    assert_eq!(
        document_reader
            .region(RegionKind::DocumentVersions)
            .expect("document-version region"),
        fixture(include_str!("fixtures/format/document_versions_one_v1.hex"))
    );
    assert_eq!(
        document_reader
            .document_version(0)
            .expect("decode document-version row"),
        Some(version)
    );

    let metadata_id = SegmentId::new(10, [0x14; 10]);
    write_segment_with_documents_and_metadata(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id: metadata_id,
            scheme: 4,
            dims: 2,
            codes: &[0x88],
            factors: SegmentFactors::Bit4(&[zeppelin_embed::quant::Bit4Factors::from_persisted(
                1.0, 1.0, 1.0,
            )]),
            rescore: &[0.0, 0.0],
            columns: &columns,
            alive: &AliveSet::new(1),
        },
        SegmentDocumentVersions {
            doc_ids: &[version.doc_id()],
            revisions: &[version.revision()],
        },
        SegmentStoredMetadata {
            end_offsets: &[4],
            bytes: b"meta",
        },
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("stored-metadata policy"),
    )
    .expect("write stored-metadata segment");
    let metadata_reader =
        SegmentReader::open(&directory.path().join(metadata_id.file_name()), metadata_id)
            .expect("open stored-metadata segment");
    assert_eq!(
        metadata_reader
            .region(RegionKind::StoredMetadata)
            .expect("stored-metadata region"),
        fixture(include_str!("fixtures/format/stored_metadata_one_v1.hex"))
    );
    assert_eq!(
        metadata_reader
            .stored_metadata()
            .expect("decode stored metadata")
            .and_then(|rows| rows.row(0)),
        Some(&b"meta"[..])
    );

    let mut text_columns = ColumnStoreBuilder::new(Schema::timestamp_only());
    for timestamp in [0, 1, 2] {
        text_columns
            .push_row(timestamp, &[])
            .expect("stored-text column row");
    }
    let text_columns = text_columns.finish().expect("stored-text columns");
    let text_id = SegmentId::new(11, [0x16; 10]);
    let text_doc_ids = [DocId::new(1), DocId::new(2), DocId::new(3)];
    let text_revisions = [Revision::new(1), Revision::new(1), Revision::new(1)];
    let text_vectors = [0.0_f32; 6];
    let text_codes = text_vectors
        .iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect::<Vec<_>>();
    write_segment_with_documents_and_text(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id: text_id,
            scheme: 0,
            dims: 2,
            codes: &text_codes,
            factors: SegmentFactors::F32,
            rescore: &text_vectors,
            columns: &text_columns,
            alive: &AliveSet::new(3),
        },
        SegmentDocumentVersions {
            doc_ids: &text_doc_ids,
            revisions: &text_revisions,
        },
        SegmentStoredText {
            present: &[1, 0, 1],
            end_offsets: &[1, 1, 3],
            bytes: b"A\xc3\xa9",
        },
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("stored-text policy"),
    )
    .expect("write stored-text segment");
    let text_reader = SegmentReader::open(&directory.path().join(text_id.file_name()), text_id)
        .expect("open stored-text segment");
    assert_eq!(
        text_reader
            .region(RegionKind::StoredText)
            .expect("stored-text region"),
        fixture(include_str!("fixtures/format/stored_text_mixed_v1.hex"))
    );
    let text_rows = text_reader
        .stored_text()
        .expect("decode stored text")
        .expect("stored text present");
    assert_eq!(text_rows.row(0), Some(Some("A")));
    assert_eq!(text_rows.row(1), Some(None));
    assert_eq!(text_rows.row(2), Some(Some("é")));
}

#[test]
fn prechange_segment_fixture_opens_without_postings() {
    let (generated, id) = golden_segment(3, 1, false);
    let frozen = fixture(include_str!(
        "fixtures/format/segment_prechange_full_v1.hex"
    ));
    assert_eq!(generated, frozen);
    let directory = tempfile::tempdir().expect("prechange segment directory");
    let path = directory.path().join(id.file_name());
    std::fs::write(&path, &frozen).expect("install prechange segment fixture");
    let reader = SegmentReader::open(&path, id).expect("open prechange segment fixture");
    reader
        .validate_all()
        .expect("validate prechange segment fixture");
    assert!(reader.postings().expect("optional postings").is_none());
}

#[test]
fn purge_intent_v1_is_byte_exact() {
    let directory = tempfile::tempdir().expect("purge-intent tempdir");
    let doc_id = DocId::new(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(doc_id, Revision::new(7)),
            vec![1.0, 0.0],
        )]))
        .expect("ingest purge-intent row");
    store.seal().expect("seal purge-intent row");
    let token = store.purge(&[doc_id]).expect("write purge intent");
    assert!(!token.is_no_op());
    assert_eq!(
        std::fs::read(directory.path().join("purge.ze")).expect("read purge intent"),
        fixture(include_str!("fixtures/format/purge_intent_v1.hex"))
    );
}

#[test]
fn manifest_clustering_range_extension_is_byte_exact() {
    let manifest = Manifest {
        generation: 8,
        log_seq: 7,
        segments: vec![
            SegmentMeta {
                id: SegmentId::new(1, [2; 10]),
                row_count: 3,
                scheme: 4,
                dims: 65,
                file_size: 99,
                epoch_id: None,
                clustering_key_range: ClusteringKeyRange::Bounded {
                    min_ts: -7,
                    max_ts: 14,
                },
            },
            SegmentMeta {
                id: SegmentId::new(2, [3; 10]),
                row_count: 4,
                scheme: 4,
                dims: 65,
                file_size: 88,
                epoch_id: None,
                clustering_key_range: ClusteringKeyRange::Empty,
            },
            SegmentMeta {
                id: SegmentId::new(3, [4; 10]),
                row_count: 5,
                scheme: 4,
                dims: 65,
                file_size: 77,
                epoch_id: None,
                clustering_key_range: ClusteringKeyRange::Unstamped,
            },
        ],
        epochs: Vec::new(),
        epoch_alias: None,
        schema: Schema::new(Vec::new()).expect("schema"),
    };

    let bytes = encode_manifest(&manifest).expect("range manifest");
    assert_eq!(
        bytes,
        fixture(include_str!(
            "fixtures/format/manifest_clustering_ranges_v2.hex"
        ))
    );
    assert_eq!(
        decode_manifest("manifest-ranges.golden", &bytes).expect("range manifest decode"),
        manifest
    );
}
