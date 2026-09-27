//! Catalog probes exercise private reconstruction/admission, not store recovery.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use zeppelin_embed::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::property_graph::catalog::{
    CatalogDeclaration, CatalogError, CatalogImage, GraphInterpretation, OnDelete,
    RelationshipRule, RelationshipRules, Symbol, SymbolCatalog, SymbolEntry, SymbolHighWaters,
    SymbolKind,
};
use zeppelin_embed::property_graph::{GraphName, StoreInstanceId};
use zeppelin_embed_adversarial_oracle::graph_catalog::{
    self as oracle, Dictionary, Interpretation,
};

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.catalog.names",
    "property-graph.catalog.duplicates",
    "property-graph.catalog.high-waters",
    "property-graph.catalog.interpretation",
    "property-graph.catalog.store-identity",
    "property-graph.catalog.cancel",
    "property-graph.catalog.cancel.fire",
    "property-graph.catalog.cancel.clean",
    "property-graph.catalog.budget.fire",
    "property-graph.catalog.budget.clean",
    "property-graph.catalog.decode-error.fire",
    "property-graph.catalog.decode-error.clean",
    "property-graph.catalog.policy.clean",
    "property-graph.catalog.policy-duplicate.fire",
];
fn waters(v: SymbolHighWaters) -> [u64; 4] {
    [v.label, v.relationship_type, v.property, v.namespace]
}
fn kind(value: u8) -> SymbolKind {
    match value {
        1 => SymbolKind::Label,
        2 => SymbolKind::RelationshipType,
        3 => SymbolKind::Property,
        _ => SymbolKind::Namespace,
    }
}
fn observe<'a>(rows: &[(u8, u64, &'a str)], high: [u64; 4]) -> Option<Dictionary<'a>> {
    let entries: Vec<_> = rows
        .iter()
        .map(|&(domain, id, name)| SymbolEntry {
            symbol: Symbol::new(kind(domain), id).expect("valid probe input"),
            name: GraphName::new(name).expect("probe name"),
        })
        .collect();
    let high = SymbolHighWaters {
        label: high[0],
        relationship_type: high[1],
        property: high[2],
        namespace: high[3],
    };
    SymbolCatalog::reconstruct(&entries, high, rows.len(), 4096, &mut || Ok(()))
        .ok()
        .map(|catalog| Dictionary {
            rows: catalog
                .entries()
                .iter()
                .map(|entry| {
                    (
                        entry.symbol.kind() as u8,
                        entry.symbol.get(),
                        entry.name.as_str(),
                    )
                })
                .collect(),
            high_waters: waters(catalog.high_waters()),
        })
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let mut rng = super::test_support::seeded_rng("property_graph::catalog_probe", seed);
    for _ in 0..8 {
        let id = rng.next_u64().max(2);
        let high = [id, 1, 0, 0];
        let rows = [(1, id, "é\0"), (1, 1, "e\u{301}\0"), (2, 1, "")];
        oracle::check_dictionary(&rows, high, observe(&rows, high))?;
        coverage.hit(REQUIRED_COVERAGE[0]);
        for duplicate in [(1, id, "other"), (1, id - 1, "é\0")] {
            let mut rows = rows.to_vec();
            rows.push(duplicate);
            oracle::check_dictionary(&rows, high, observe(&rows, high))?;
        }
        coverage.hit(REQUIRED_COVERAGE[1]);
        let requests = [(1, "a"), (1, "b"), (1, "a"), (2, "a"), (3, "a"), (4, "")];
        let initial = [u64::MAX - 1, 0, id, 0];
        let mut catalog = SymbolCatalog::reconstruct(
            &[],
            SymbolHighWaters {
                label: initial[0],
                relationship_type: 0,
                property: id,
                namespace: 0,
            },
            requests.len(),
            4096,
            &mut || Ok(()),
        )
        .map_err(|e| e.to_string())?;
        let mut observed = Vec::new();
        for (domain, name) in requests {
            observed.push(
                match catalog.intern(
                    kind(domain),
                    GraphName::new(name).map_err(|e| e.to_string())?,
                    &mut || Ok(()),
                ) {
                    Ok(symbol) => Some(symbol.get()),
                    Err(CatalogError::SymbolOverflow) => None,
                    Err(error) => return Err(error.to_string()),
                },
            );
        }
        oracle::check_interning(&requests, initial, &observed, waters(catalog.high_waters()))?;
        coverage.hit(REQUIRED_COVERAGE[2]);
        let lexical = TokenizerConfig::text_default().epoch();
        let tower = EmbeddingTower {
            model_id: "document".into(),
            model_version: "1".into(),
            weights_digest: vec![1],
            dims: 2,
            normalization: Normalization::None,
            prompt_prefix: String::new(),
            max_tokens: 1,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        };
        let stored = GraphInterpretation::new(lexical, Some(&tower)).map_err(|e| e.to_string())?;
        let primitive = Interpretation {
            lexical: lexical.value(),
            document: Some(("document", 2)),
        };
        for (model, dims, present) in [
            ("document", 2, true),
            ("Document", 2, true),
            ("document", 3, true),
            ("document", 2, false),
        ] {
            let mut changed = tower.clone();
            changed.model_id = model.into();
            changed.dims = dims;
            let declaration = GraphInterpretation::new(lexical, present.then_some(&changed))
                .map_err(|e| e.to_string())?;
            oracle::check_admission(
                primitive,
                Interpretation {
                    lexical: lexical.value(),
                    document: present.then_some((model, dims)),
                },
                stored.validate_for(declaration, &mut || Ok(())).is_ok(),
            )?;
        }
        coverage.hit(REQUIRED_COVERAGE[3]);
        let store = StoreInstanceId::new((u128::from(id) << 64) | 1).map_err(|e| e.to_string())?;
        let rules = [RelationshipRule {
            relationship_type: GraphName::new("IN").map_err(|e| e.to_string())?,
            on_delete: OnDelete::Cascade,
        }];
        if !matches!(
            RelationshipRules::new(&[rules[0], rules[0]]),
            Err(CatalogError::Duplicate)
        ) {
            return Err("PG4 duplicate relationship policy did not reject".into());
        }
        coverage.hit("property-graph.catalog.policy-duplicate.fire");
        let image = CatalogImage {
            relationship_rules: RelationshipRules::new(&rules).map_err(|e| e.to_string())?,
            declaration: CatalogDeclaration {
                store,
                node_high_water: u128::from(id) << 64,
                relationship_high_water: 0,
                interpretation: stored,
            },
            symbols: catalog,
        };
        let mut bytes = vec![
            0;
            image
                .encoded_len(&mut || Ok(()))
                .map_err(|e| e.to_string())?
        ];
        image
            .encode_into(&mut bytes, &mut || Ok(()))
            .map_err(|e| e.to_string())?;
        let restored =
            CatalogImage::decode(&bytes, 4096, &mut || Ok(())).map_err(|e| e.to_string())?;
        if restored
            .relationship_rules
            .lookup(rules[0].relationship_type, &mut || Ok(()))
            .map_err(|e| e.to_string())?
            != Some(OnDelete::Cascade)
        {
            return Err("PG4 persisted relationship policy changed".into());
        }
        coverage.hit("property-graph.catalog.policy.clean");
        if restored.declaration != image.declaration
            || restored.symbols.high_waters() != image.symbols.high_waters()
        {
            return Err("PG4 roundtrip changed store/counters/declaration".into());
        }
        if restored.declaration.validate_for(
            StoreInstanceId::new(1).map_err(|e| e.to_string())?,
            stored,
            &mut || Ok(()),
        ) != Err(CatalogError::StoreMismatch)
        {
            return Err("PG4 wrong store admitted".into());
        }
        let rows: Vec<_> = image
            .symbols
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.symbol.kind() as u8,
                    entry.symbol.get(),
                    entry.name.as_str(),
                )
            })
            .collect();
        let observed = Dictionary {
            rows: restored
                .symbols
                .entries()
                .iter()
                .map(|entry| {
                    (
                        entry.symbol.kind() as u8,
                        entry.symbol.get(),
                        entry.name.as_str(),
                    )
                })
                .collect(),
            high_waters: waters(restored.symbols.high_waters()),
        };
        oracle::check_dictionary(&rows, waters(image.symbols.high_waters()), Some(observed))?;
        coverage.hit(REQUIRED_COVERAGE[4]);
        // The same seed and byte image supply every clean/fault pair. Measure the
        // successful traversal, then cancel halfway through real decode work.
        let mut clean_calls = 0;
        let clean = CatalogImage::decode(&bytes, 4096, &mut || {
            clean_calls += 1;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
        if clean_calls <= 4
            || clean.declaration != restored.declaration
            || clean.symbols.entries() != restored.symbols.entries()
            || clean.symbols.high_waters() != restored.symbols.high_waters()
        {
            return Err("PG4 cancellation clean control changed contents/work".into());
        }
        coverage.hit("property-graph.catalog.cancel.clean");
        let trip = clean_calls / 2;
        let mut calls = 0;
        let mut fired = 0;
        let cancelled = CatalogImage::decode(&bytes, 4096, &mut || {
            calls += 1;
            if calls == trip {
                fired += 1;
                Err(CatalogError::Cancelled)
            } else {
                Ok(())
            }
        });
        if !matches!(cancelled, Err(CatalogError::Cancelled)) || fired != 1 || calls != trip {
            return Err(format!(
                "PG4 in-work cancellation ignored: fired={fired}, calls={calls}, trip={trip}"
            ));
        }
        coverage.hit("property-graph.catalog.cancel.fire");
        coverage.hit(REQUIRED_COVERAGE[5]);
        // Nonempty dictionaries need descriptors. Zero their caller allowance,
        // then restore it for the paired successful reconstruction.
        if !matches!(
            CatalogImage::decode(&bytes, 0, &mut || Ok(())),
            Err(CatalogError::Capacity)
        ) {
            return Err("PG4 descriptor budget fault did not fire".into());
        }
        coverage.hit("property-graph.catalog.budget.fire");
        let mut damaged = bytes.clone();
        let last = damaged.last_mut().ok_or("PG4 missing checksum")?;
        *last ^= 1;
        if !matches!(
            CatalogImage::decode(&damaged, 4096, &mut || Ok(())),
            Err(CatalogError::Checksum)
        ) {
            return Err("PG4 checksum fault did not fire".into());
        }
        coverage.hit("property-graph.catalog.decode-error.fire");
        for key in [
            "property-graph.catalog.budget.clean",
            "property-graph.catalog.decode-error.clean",
        ] {
            let clean =
                CatalogImage::decode(&bytes, 4096, &mut || Ok(())).map_err(|e| e.to_string())?;
            if clean.declaration != restored.declaration
                || clean.symbols.entries() != restored.symbols.entries()
                || clean.symbols.high_waters() != restored.symbols.high_waters()
            {
                return Err(format!("PG4 clean control changed contents: {key}"));
            }
            coverage.hit(key);
        }
    }
    Ok(())
}
