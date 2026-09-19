#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use zeppelin_embed::property_graph::GraphName;
use zeppelin_embed::property_graph::catalog::{
    CatalogError, LabelId, NamespaceId, PropertyKeyId, RelTypeId,
};
use zeppelin_embed::property_graph::catalog::{
    Symbol, SymbolCatalog, SymbolEntry, SymbolHighWaters, SymbolKind,
};

fn fixture(text: &str) -> Vec<u8> {
    text.split_whitespace()
        .flat_map(|line| {
            (0..line.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&line[i..i + 2], 16).unwrap())
        })
        .collect()
}
fn plain_golden() -> Vec<u8> {
    fixture(include_str!(
        "fixtures/format/graph_catalog_without_embedding_v1.hex"
    ))
}
fn document_golden() -> Vec<u8> {
    fixture(include_str!(
        "fixtures/format/graph_catalog_document_v1.hex"
    ))
}
fn repair(bytes: &mut [u8]) {
    let end = bytes.len() - 8;
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[..end]);
    bytes[end..].copy_from_slice(&checksum.to_le_bytes());
}

#[test]
fn catalog_independent_goldens_pin_full_identity_and_exact_document_declaration() {
    use zeppelin_embed::property_graph::catalog::{CatalogImage, DocumentDeclaration};
    let document = tower();
    for (bytes, present) in [(plain_golden(), false), (document_golden(), true)] {
        let value = CatalogImage::decode(&bytes, 4096, &mut || Ok(())).unwrap();
        assert_eq!(value.declaration.store.get(), (1_u128 << 100) + 7);
        assert_eq!(value.declaration.node_high_water, u128::MAX);
        assert_eq!(
            value.declaration.relationship_high_water,
            (1_u128 << 64) + 1
        );
        assert_eq!(
            value.declaration.interpretation.lexical().value(),
            0x123456789abcdef0
        );
        assert_eq!(
            value.declaration.interpretation.embedding(),
            present.then(|| DocumentDeclaration::new(&document).unwrap())
        );
        assert_eq!(
            value.symbols.high_waters(),
            SymbolHighWaters {
                label: 9,
                relationship_type: u64::MAX,
                property: 17,
                namespace: 1
            }
        );
        let got: Vec<_> = value
            .symbols
            .entries()
            .iter()
            .map(|entry| (entry.symbol.kind(), entry.symbol.get(), entry.name.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (SymbolKind::Label, 9, ""),
                (SymbolKind::RelationshipType, u64::MAX, "R"),
                (SymbolKind::Property, 17, "é\0"),
                (SymbolKind::Namespace, 1, "namespace")
            ]
        );
        let mut encoded = vec![0xa5; bytes.len()];
        value.encode_into(&mut encoded, &mut || Ok(())).unwrap();
        assert_eq!(encoded, bytes);
    }
}

#[test]
fn catalog_document_corruption_rejects_unknown_enums_optional_tags_and_lengths() {
    use zeppelin_embed::property_graph::catalog::CatalogImage;
    let golden = document_golden();
    for (offset, bytes) in [
        (120, u64::MAX.to_le_bytes().to_vec()),
        (156, 0_u32.to_le_bytes().to_vec()),
        (160, 99_u16.to_le_bytes().to_vec()),
        (179, 99_u16.to_le_bytes().to_vec()),
        (181, 99_u16.to_le_bytes().to_vec()),
        (183, vec![2]),
        (192, vec![255]),
    ] {
        let mut bad = golden.clone();
        bad[offset..offset + bytes.len()].copy_from_slice(&bytes);
        repair(&mut bad);
        assert!(
            CatalogImage::decode(&bad, 4096, &mut || Ok(())).is_err(),
            "offset {offset}"
        );
    }
}

#[test]
fn catalog_budget_and_cancellation_refusals_preserve_private_state() {
    use zeppelin_embed::property_graph::catalog::CatalogImage;
    let bytes = document_golden();
    assert_eq!(
        CatalogImage::decode(&bytes, 0, &mut || Ok(())).unwrap_err(),
        CatalogError::Capacity
    );
    assert_eq!(
        CatalogImage::decode(&bytes, 4096, &mut || Err(CatalogError::Cancelled)).unwrap_err(),
        CatalogError::Cancelled
    );
    let image = CatalogImage::decode(&bytes, 4096, &mut || Ok(())).unwrap();
    let mut output = vec![0xa5; bytes.len() - 1];
    assert_eq!(
        image.encode_into(&mut output, &mut || Ok(())),
        Err(CatalogError::Capacity)
    );
    assert!(output.iter().all(|b| *b == 0xa5));
    output.push(0xa5);
    let mut calls = 0;
    assert_eq!(
        image.encode_into(&mut output, &mut || {
            calls += 1;
            if calls == 10 {
                Err(CatalogError::Cancelled)
            } else {
                Ok(())
            }
        }),
        Err(CatalogError::Cancelled)
    );
    assert_eq!(calls, 10);
    assert_eq!(&output[..4], b"ZGCA");
    let mut calls = 0;
    assert_eq!(
        CatalogImage::decode(&bytes, 4096, &mut || {
            calls += 1;
            if calls == 6 {
                Err(CatalogError::Cancelled)
            } else {
                Ok(())
            }
        })
        .unwrap_err(),
        CatalogError::Cancelled
    );
    assert_eq!(calls, 6);
    let mut symbols =
        SymbolCatalog::reconstruct(&[], SymbolHighWaters::default(), 1, 1024, &mut || Ok(()))
            .unwrap();
    let name = GraphName::new("required").unwrap();
    let mut calls = 0;
    assert_eq!(
        symbols.intern(SymbolKind::Label, name, &mut || {
            calls += 1;
            if calls == 2 {
                Err(CatalogError::Cancelled)
            } else {
                Ok(())
            }
        }),
        Err(CatalogError::Cancelled)
    );
    assert!(symbols.entries().is_empty());
    assert_eq!(symbols.high_waters(), SymbolHighWaters::default());
    let assigned = symbols
        .intern(SymbolKind::Label, name, &mut || Ok(()))
        .unwrap();
    assert_eq!(
        symbols
            .intern(SymbolKind::Label, name, &mut || Ok(()))
            .unwrap(),
        assigned
    );
    assert_eq!(
        symbols.intern(
            SymbolKind::Label,
            GraphName::new("other").unwrap(),
            &mut || Ok(())
        ),
        Err(CatalogError::Capacity)
    );
    assert_eq!(symbols.entries().len(), 1);
    assert_eq!(symbols.high_waters().label, 1);
}

#[test]
fn catalog_optional_vectors_are_not_required_and_query_towers_do_not_change_documents() {
    use zeppelin_embed::epoch::EmbeddingEpoch;
    use zeppelin_embed::fts::tokenizer::TokenizerConfig;
    use zeppelin_embed::property_graph::CanonicalEmbedding;
    use zeppelin_embed::property_graph::catalog::{DocumentDeclaration, GraphInterpretation};
    let document = tower();
    let lexical = TokenizerConfig::text_default().epoch();
    let declared = GraphInterpretation::new(lexical, Some(&document)).unwrap();
    let absent = GraphInterpretation::new(lexical, None).unwrap();
    let payload = CanonicalEmbedding::new(&document, &[1.0, 2.0]).unwrap();
    assert_eq!(declared.validate_payload(None, &mut || Ok(())), Ok(()));
    assert_eq!(absent.validate_payload(None, &mut || Ok(())), Ok(()));
    assert_eq!(
        declared.validate_payload(Some(payload), &mut || Ok(())),
        Ok(())
    );
    assert_eq!(
        absent.validate_payload(Some(payload), &mut || Ok(())),
        Err(CatalogError::NoEmbeddingSpace)
    );
    let mut other = tower();
    other.model_id.push('x');
    assert_eq!(
        declared.validate_payload(
            Some(CanonicalEmbedding::new(&other, &[1.0, 2.0]).unwrap()),
            &mut || Ok(())
        ),
        Err(CatalogError::InterpretationMismatch)
    );
    let mut epoch = EmbeddingEpoch {
        document: document.clone(),
        query: document.clone(),
        alignment_digest: vec![],
    };
    epoch.query.model_id.push('q');
    epoch.alignment_digest.push(9);
    assert_eq!(
        declared.validate_for(
            GraphInterpretation::new(lexical, Some(&epoch.document)).unwrap(),
            &mut || Ok(())
        ),
        Ok(())
    );
    assert_eq!(declared.embedding().unwrap().dimensions(), 2);
    for dims in [0, u32::MAX] {
        let mut invalid = tower();
        invalid.dims = dims;
        assert_eq!(
            DocumentDeclaration::new(&invalid),
            Err(CatalogError::InvalidEmbedding)
        );
    }
    let mut invalid = tower();
    invalid.model_id = "x".repeat(8 * 1024 * 1024);
    assert_eq!(
        DocumentDeclaration::new(&invalid),
        Err(CatalogError::InvalidEmbedding)
    );
}

#[test]
fn catalog_corruption_refuses_truncation_tags_overflow_duplicates_and_trailing_bytes() {
    use zeppelin_embed::property_graph::catalog::CatalogImage;
    let golden = plain_golden();
    for end in 0..golden.len() {
        assert!(
            CatalogImage::decode(&golden[..end], 4096, &mut || Ok(())).is_err(),
            "truncation {end}"
        );
    }
    let mutations: Vec<(usize, Vec<u8>, CatalogError)> = vec![
        (0, b"FAIL".to_vec(), CatalogError::Malformed),
        (4, 2_u16.to_le_bytes().to_vec(), CatalogError::Unsupported),
        (6, 2_u16.to_le_bytes().to_vec(), CatalogError::Unsupported),
        (8, u64::MAX.to_le_bytes().to_vec(), CatalogError::Malformed),
        (16, vec![0; 16], CatalogError::Malformed),
        (64, 0_u64.to_le_bytes().to_vec(), CatalogError::HighWater),
        (
            104,
            u64::MAX.to_le_bytes().to_vec(),
            CatalogError::Malformed,
        ),
        (104, 0_u64.to_le_bytes().to_vec(), CatalogError::Malformed),
        (112, vec![2], CatalogError::Unsupported),
        (113, vec![1], CatalogError::Malformed),
        (120, vec![99], CatalogError::Unsupported),
        (121, vec![1], CatalogError::Malformed),
        (128, vec![0; 8], CatalogError::ZeroSymbol),
        (
            136,
            u64::MAX.to_le_bytes().to_vec(),
            CatalogError::Malformed,
        ),
        (168, vec![255], CatalogError::Malformed),
    ];
    for (offset, replacement, expected) in mutations {
        let mut bad = golden.clone();
        bad[offset..offset + replacement.len()].copy_from_slice(&replacement);
        repair(&mut bad);
        assert_eq!(
            CatalogImage::decode(&bad, 4096, &mut || Ok(())).unwrap_err(),
            expected,
            "offset {offset}"
        );
    }
    let mut bad = golden.clone();
    bad[168] ^= 1;
    assert_eq!(
        CatalogImage::decode(&bad, 4096, &mut || Ok(())).unwrap_err(),
        CatalogError::Checksum
    );
    let mut bad = golden.clone();
    bad.push(0);
    let length = bad.len() as u64;
    bad[8..16].copy_from_slice(&length.to_le_bytes());
    repair(&mut bad);
    assert!(CatalogImage::decode(&bad, 4096, &mut || Ok(())).is_err());
    // Two well-framed assignments in one domain can collide by id or by name.
    for duplicate_id in [false, true] {
        let mut bytes = golden[..120].to_vec();
        bytes[104..112].copy_from_slice(&2_u64.to_le_bytes());
        for (id, name) in [
            (1_u64, b'a'),
            (
                if duplicate_id { 1 } else { 2 },
                if duplicate_id { b'b' } else { b'a' },
            ),
        ] {
            bytes.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&1_u64.to_le_bytes());
            bytes.push(name);
        }
        bytes.extend_from_slice(&[0; 8]);
        let length = bytes.len() as u64;
        bytes[8..16].copy_from_slice(&length.to_le_bytes());
        repair(&mut bytes);
        assert_eq!(
            CatalogImage::decode(&bytes, 4096, &mut || Ok(())).unwrap_err(),
            CatalogError::Duplicate
        );
    }
}

fn tower() -> zeppelin_embed::epoch::EmbeddingTower {
    use zeppelin_embed::epoch::*;
    EmbeddingTower {
        model_id: "model\0x".into(),
        model_version: "v1".into(),
        weights_digest: vec![1, 2, 3],
        dims: 2,
        normalization: Normalization::L2,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: Some("build".into()),
    }
}

#[test]
fn catalog_long_name_lookup_observes_cancellation_during_comparison() {
    let name = "x".repeat(192 * 1024);
    let entries = [SymbolEntry {
        symbol: Symbol::Label(LabelId::new(1).unwrap()),
        name: GraphName::new(&name).unwrap(),
    }];
    let catalog = SymbolCatalog::reconstruct(
        &entries,
        SymbolHighWaters {
            label: 1,
            ..Default::default()
        },
        1,
        4096,
        &mut || Ok(()),
    )
    .unwrap();
    let mut calls = 0;
    assert_eq!(
        catalog.lookup(
            SymbolKind::Label,
            GraphName::new(&name).unwrap(),
            &mut || {
                calls += 1;
                if calls == 5 {
                    Err(CatalogError::Cancelled)
                } else {
                    Ok(())
                }
            }
        ),
        Err(CatalogError::Cancelled)
    );
    assert_eq!(calls, 5);
}

#[test]
fn catalog_long_interpretation_observes_cancellation_during_exact_comparison() {
    use zeppelin_embed::fts::tokenizer::TokenizerConfig;
    use zeppelin_embed::property_graph::catalog::GraphInterpretation;
    let mut document = tower();
    document.model_id = "x".repeat(192 * 1024);
    let interpretation =
        GraphInterpretation::new(TokenizerConfig::text_default().epoch(), Some(&document)).unwrap();
    let short = tower();
    let short_interpretation =
        GraphInterpretation::new(TokenizerConfig::text_default().epoch(), Some(&short)).unwrap();
    let mut short_calls = 0;
    short_interpretation
        .validate_for(short_interpretation, &mut || {
            short_calls += 1;
            Ok(())
        })
        .unwrap();
    let mut calls = 0;
    assert_eq!(
        interpretation.validate_for(interpretation, &mut || {
            calls += 1;
            if calls > short_calls {
                Err(CatalogError::Cancelled)
            } else {
                Ok(())
            }
        }),
        Err(CatalogError::Cancelled)
    );
    assert_eq!(calls, short_calls + 1);
}

#[test]
fn catalog_reconstruction_cancels_during_long_shared_prefix_sorting() {
    let short = ["a", "b"];
    let long = [
        format!("{}a", "x".repeat(192 * 1024)),
        format!("{}b", "x".repeat(192 * 1024)),
    ];
    fn rows<'a>(names: &[&'a str; 2]) -> [SymbolEntry<'a>; 2] {
        [
            SymbolEntry {
                symbol: Symbol::Label(LabelId::new(2).unwrap()),
                name: GraphName::new(names[1]).unwrap(),
            },
            SymbolEntry {
                symbol: Symbol::Label(LabelId::new(1).unwrap()),
                name: GraphName::new(names[0]).unwrap(),
            },
        ]
    }
    let mut short_calls = 0;
    SymbolCatalog::reconstruct(
        &rows(&short),
        SymbolHighWaters {
            label: 2,
            ..Default::default()
        },
        2,
        4096,
        &mut || {
            short_calls += 1;
            Ok(())
        },
    )
    .unwrap();
    let mut calls = 0;
    assert_eq!(
        SymbolCatalog::reconstruct(
            &rows(&[&long[0], &long[1]]),
            SymbolHighWaters {
                label: 2,
                ..Default::default()
            },
            2,
            4096,
            &mut || {
                calls += 1;
                if calls > short_calls {
                    Err(CatalogError::Cancelled)
                } else {
                    Ok(())
                }
            }
        )
        .unwrap_err(),
        CatalogError::Cancelled
    );
}

#[test]
fn catalog_utf8_spanning_work_chunks_is_preserved_and_invalid_continuations_fail() {
    use zeppelin_embed::fts::tokenizer::TokenizerConfig;
    use zeppelin_embed::property_graph::StoreInstanceId;
    use zeppelin_embed::property_graph::catalog::{
        CatalogDeclaration, CatalogImage, GraphInterpretation,
    };
    for split in 1..4 {
        let name = format!("{}😀tail", "a".repeat(64 * 1024 - split));
        let mut symbols =
            SymbolCatalog::reconstruct(&[], SymbolHighWaters::default(), 1, 4096, &mut || Ok(()))
                .unwrap();
        symbols
            .intern(
                SymbolKind::Label,
                GraphName::new(&name).unwrap(),
                &mut || Ok(()),
            )
            .unwrap();
        let image = CatalogImage {
            declaration: CatalogDeclaration {
                store: StoreInstanceId::new(1).unwrap(),
                node_high_water: 0,
                relationship_high_water: 0,
                interpretation: GraphInterpretation::new(
                    TokenizerConfig::text_default().epoch(),
                    None,
                )
                .unwrap(),
            },
            symbols,
        };
        let mut encoded = vec![0; image.encoded_len(&mut || Ok(())).unwrap()];
        image.encode_into(&mut encoded, &mut || Ok(())).unwrap();
        let restored = CatalogImage::decode(&encoded, 4096, &mut || Ok(())).unwrap();
        assert_eq!(restored.symbols.entries()[0].name.as_str(), name);
        let mut bad = encoded.clone();
        bad[144 + 64 * 1024 - split + 1] = 0xff;
        repair(&mut bad);
        assert_eq!(
            CatalogImage::decode(&bad, 4096, &mut || Ok(())).unwrap_err(),
            CatalogError::Malformed
        );
    }
}

#[test]
fn logical_catalog_roundtrip_preserves_store_symbols_counters_and_optional_space() {
    use zeppelin_embed::fts::tokenizer::TokenizerConfig;
    use zeppelin_embed::property_graph::StoreInstanceId;
    use zeppelin_embed::property_graph::catalog::{
        CatalogDeclaration, CatalogImage, GraphInterpretation,
    };
    let tower = tower();
    let store = StoreInstanceId::new((1_u128 << 100) + 7).unwrap();
    for document in [None, Some(&tower)] {
        let mut symbols =
            SymbolCatalog::reconstruct(&[], SymbolHighWaters::default(), 3, 4096, &mut || Ok(()))
                .unwrap();
        let label = symbols
            .intern(
                SymbolKind::Label,
                GraphName::new("é\0").unwrap(),
                &mut || Ok(()),
            )
            .unwrap();
        let property = symbols
            .intern(
                SymbolKind::Property,
                GraphName::new("ts").unwrap(),
                &mut || Ok(()),
            )
            .unwrap();
        let image = CatalogImage {
            declaration: CatalogDeclaration {
                store,
                node_high_water: u128::MAX,
                relationship_high_water: (1_u128 << 100) + 3,
                interpretation: GraphInterpretation::new(
                    TokenizerConfig::text_default().epoch(),
                    document,
                )
                .unwrap(),
            },
            symbols,
        };
        let mut bytes = vec![0xa5; image.encoded_len(&mut || Ok(())).unwrap()];
        assert_eq!(
            image.encode_into(&mut bytes, &mut || Ok(())).unwrap(),
            bytes.len()
        );
        let reconstructed = CatalogImage::decode(&bytes, 4096, &mut || Ok(())).unwrap();
        assert_eq!(reconstructed.declaration, image.declaration);
        assert_eq!(reconstructed.symbols.entries(), image.symbols.entries());
        assert_eq!(
            reconstructed
                .symbols
                .name(label, &mut || Ok(()))
                .unwrap()
                .unwrap()
                .as_str(),
            "é\0"
        );
        assert_eq!(
            reconstructed
                .symbols
                .name(property, &mut || Ok(()))
                .unwrap()
                .unwrap()
                .as_str(),
            "ts"
        );
        let mut relocated = vec![0; bytes.len()];
        reconstructed
            .encode_into(&mut relocated, &mut || Ok(()))
            .unwrap();
        assert_eq!(relocated, bytes);
        assert_eq!(
            reconstructed.declaration.validate_for(
                store,
                image.declaration.interpretation,
                &mut || Ok(())
            ),
            Ok(())
        );
        assert_eq!(
            reconstructed.declaration.validate_for(
                StoreInstanceId::new(7).unwrap(),
                image.declaration.interpretation,
                &mut || Ok(())
            ),
            Err(CatalogError::StoreMismatch)
        );
    }
}

#[test]
fn catalog_admission_refuses_every_changed_document_field_or_analyzer() {
    use zeppelin_embed::epoch::*;
    use zeppelin_embed::fts::tokenizer::TokenizerConfig;
    use zeppelin_embed::property_graph::catalog::GraphInterpretation;
    let analyzer = TokenizerConfig::text_default().epoch();
    let original = tower();
    let stored = GraphInterpretation::new(analyzer, Some(&original)).unwrap();
    assert_eq!(stored.validate_for(stored, &mut || Ok(())), Ok(()));
    let mut changed = original.clone();
    changed.model_id.push('x');
    assert_eq!(
        stored.validate_for(
            GraphInterpretation::new(analyzer, Some(&changed)).unwrap(),
            &mut || Ok(())
        ),
        Err(CatalogError::InterpretationMismatch)
    );
    for field in 0..10 {
        let mut changed = original.clone();
        match field {
            0 => changed.model_id.push('x'),
            1 => changed.model_version.push('x'),
            2 => changed.weights_digest.push(4),
            3 => changed.dims = 3,
            4 => changed.normalization = Normalization::None,
            5 => changed.prompt_prefix.push('x'),
            6 => changed.max_tokens = 33,
            7 => changed.runtime = EmbeddingRuntime::Mlx,
            8 => changed.compute_units = ComputeUnits::All,
            _ => changed.os_build = None,
        }
        assert_eq!(
            stored.validate_for(
                GraphInterpretation::new(analyzer, Some(&changed)).unwrap(),
                &mut || Ok(())
            ),
            Err(CatalogError::InterpretationMismatch),
            "field {field}"
        );
    }
    let without = GraphInterpretation::new(analyzer, None).unwrap();
    assert_eq!(without.validate_for(without, &mut || Ok(())), Ok(()));
    assert_eq!(
        without.validate_for(stored, &mut || Ok(())),
        Err(CatalogError::InterpretationMismatch)
    );
    assert_eq!(
        stored.validate_for(without, &mut || Ok(())),
        Err(CatalogError::InterpretationMismatch)
    );
    let mut config = TokenizerConfig::text_default();
    config.number_words = !config.number_words;
    assert_eq!(
        without.validate_for(
            GraphInterpretation::new(config.epoch(), None).unwrap(),
            &mut || Ok(())
        ),
        Err(CatalogError::InterpretationMismatch)
    );
}

#[test]
fn catalog_reconstruction_preserves_exact_names_domains_and_high_waters() {
    let entries = [
        SymbolEntry {
            symbol: Symbol::Label(LabelId::new(17).unwrap()),
            name: GraphName::new("é\0").unwrap(),
        },
        SymbolEntry {
            symbol: Symbol::Property(PropertyKeyId::new(17).unwrap()),
            name: GraphName::new("é\0").unwrap(),
        },
        SymbolEntry {
            symbol: Symbol::Label(LabelId::new(2).unwrap()),
            name: GraphName::new("e\u{301}\0").unwrap(),
        },
        SymbolEntry {
            symbol: Symbol::Namespace(NamespaceId::new(1).unwrap()),
            name: GraphName::new("").unwrap(),
        },
    ];
    let waters = SymbolHighWaters {
        label: 23,
        property: 17,
        namespace: 1,
        relationship_type: u64::MAX,
    };
    let mut catalog =
        SymbolCatalog::reconstruct(&entries, waters, 8, 4096, &mut || Ok(())).unwrap();
    for entry in entries {
        assert_eq!(
            catalog
                .lookup(entry.symbol.kind(), entry.name, &mut || Ok(()))
                .unwrap(),
            Some(entry.symbol)
        );
        assert_eq!(
            catalog.name(entry.symbol, &mut || Ok(())).unwrap(),
            Some(entry.name)
        );
    }
    assert_eq!(catalog.high_waters(), waters);
    assert_eq!(
        catalog
            .intern(
                SymbolKind::Label,
                GraphName::new("new").unwrap(),
                &mut || Ok(())
            )
            .unwrap(),
        Symbol::Label(LabelId::new(24).unwrap())
    );
    assert_eq!(
        catalog
            .intern(
                SymbolKind::Label,
                GraphName::new("é\0").unwrap(),
                &mut || Ok(())
            )
            .unwrap(),
        entries[0].symbol
    );
    assert_eq!(catalog.high_waters().label, 24);
    assert_eq!(
        catalog.intern(
            SymbolKind::RelationshipType,
            GraphName::new("new").unwrap(),
            &mut || Ok(())
        ),
        Err(CatalogError::SymbolOverflow)
    );
    assert_eq!(catalog.entries().len(), 5);
    let relocated: Vec<_> = catalog.entries().iter().rev().copied().collect();
    let copy =
        SymbolCatalog::reconstruct(&relocated, catalog.high_waters(), 5, 4096, &mut || Ok(()))
            .unwrap();
    assert_eq!(copy.entries().len(), catalog.entries().len());
    for entry in catalog.entries() {
        assert_eq!(
            copy.lookup(entry.symbol.kind(), entry.name, &mut || Ok(()))
                .unwrap(),
            Some(entry.symbol)
        );
    }
}

#[test]
fn catalog_rejects_duplicate_assignments_and_regressed_high_waters() {
    let entry = SymbolEntry {
        symbol: Symbol::Label(LabelId::new(1).unwrap()),
        name: GraphName::new("n").unwrap(),
    };
    let waters = SymbolHighWaters {
        label: 2,
        ..Default::default()
    };
    let impossible_capacity = (isize::MAX as usize) / std::mem::size_of::<SymbolEntry<'_>>() + 1;
    assert!(matches!(
        SymbolCatalog::reconstruct(&[], waters, impossible_capacity, usize::MAX, &mut || Ok(())),
        Err(CatalogError::Allocation)
    ));
    for second in [
        entry,
        SymbolEntry {
            name: GraphName::new("other").unwrap(),
            ..entry
        },
        SymbolEntry {
            symbol: Symbol::Label(LabelId::new(2).unwrap()),
            ..entry
        },
    ] {
        assert!(matches!(
            SymbolCatalog::reconstruct(&[entry, second], waters, 2, 4096, &mut || Ok(())),
            Err(CatalogError::Duplicate)
        ));
    }
    assert!(matches!(
        SymbolCatalog::reconstruct(&[entry], SymbolHighWaters::default(), 1, 4096, &mut || Ok(
            ()
        )),
        Err(CatalogError::HighWater)
    ));
    assert!(matches!(
        SymbolCatalog::reconstruct(&[entry], waters, 0, 4096, &mut || Ok(())),
        Err(CatalogError::Capacity)
    ));
    assert!(matches!(
        SymbolCatalog::reconstruct(&[entry], waters, usize::MAX, usize::MAX, &mut || Ok(())),
        Err(CatalogError::Capacity)
    ));
    assert!(matches!(
        SymbolCatalog::reconstruct(&[entry], waters, 1, 0, &mut || Ok(())),
        Err(CatalogError::Capacity)
    ));
    assert!(matches!(
        SymbolCatalog::reconstruct(&[entry], waters, 1, 4096, &mut || Err(
            CatalogError::Cancelled
        )),
        Err(CatalogError::Cancelled)
    ));
}

#[test]
fn catalog_symbols_preserve_full_width_and_refuse_zero_or_overflow() {
    assert_eq!(LabelId::new(0), Err(CatalogError::ZeroSymbol));
    assert_eq!(RelTypeId::new(0), Err(CatalogError::ZeroSymbol));
    assert_eq!(PropertyKeyId::new(0), Err(CatalogError::ZeroSymbol));
    assert_eq!(NamespaceId::new(0), Err(CatalogError::ZeroSymbol));
    assert_eq!(LabelId::new(u64::MAX).unwrap().get(), u64::MAX);
    assert_eq!(RelTypeId::new(u64::MAX).unwrap().get(), u64::MAX);
    assert_eq!(PropertyKeyId::new(u64::MAX).unwrap().get(), u64::MAX);
    assert_eq!(NamespaceId::new(u64::MAX).unwrap().get(), u64::MAX);
    assert_eq!(
        LabelId::new(u64::MAX).unwrap().next(),
        Err(CatalogError::SymbolOverflow)
    );
    assert_eq!(
        RelTypeId::new(u64::MAX).unwrap().next(),
        Err(CatalogError::SymbolOverflow)
    );
    assert_eq!(
        PropertyKeyId::new(u64::MAX).unwrap().next(),
        Err(CatalogError::SymbolOverflow)
    );
    assert_eq!(
        NamespaceId::new(u64::MAX).unwrap().next(),
        Err(CatalogError::SymbolOverflow)
    );
    assert_eq!(LabelId::new(1).unwrap().next().unwrap().get(), 2);
    assert_eq!(RelTypeId::new(1).unwrap().next().unwrap().get(), 2);
    assert_eq!(PropertyKeyId::new(1).unwrap().next().unwrap().get(), 2);
    assert_eq!(NamespaceId::new(1).unwrap().next().unwrap().get(), 2);
}
