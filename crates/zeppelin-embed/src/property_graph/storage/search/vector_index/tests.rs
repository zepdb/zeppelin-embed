#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "test fixtures use assertions and checked fixed indices"
)]
use super::test_support::{
    InspectVectorSources, SourceReport, apply_repeated_vectors_checked, apply_vector_checked,
    document_tower, inspect_sources_checked, kernel_coordinates, native_options,
    prepare_small_writes_fixture, read_kernel, try_apply_repeated_vectors, write_and_read_kernel,
};
use crate::epoch::{EmbeddingTower, Normalization};
use crate::graph::search::{GraphSearchRequest, GraphSearchScratch, GraphSearcher};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::{NativeReadConsumer, NativeReadLease};
use crate::lifecycle::{CancelToken, QueryControl, Store};
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::resources::GraphResources;
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::artifact::{ArtifactId, BlockKind, PhysicalRef};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::storage::{
    GraphReadView, NativeCatalog, NativeQuerySource, NativeReadCapability,
};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphDeleteMode,
    GraphName, GraphRevision, NodeId,
};
use xxhash_rust::xxh3::xxh3_64;

fn apply_vector(
    store: &Store,
    document: &EmbeddingTower,
    key: &str,
    revision: u64,
    operation: StructuredOperation,
    coordinates: &[f32; 2],
) -> NodeId {
    apply_vector_checked(store, document, key, revision, operation, coordinates)
        .expect("vector write")
}

fn apply_repeated_vectors(
    store: &Store,
    document: &EmbeddingTower,
    prefix: &str,
    count: usize,
    coordinates: &[f32],
) -> Vec<NodeId> {
    apply_repeated_vectors_checked(store, document, prefix, count, coordinates)
        .expect("vector batch")
}

#[test]
fn native_vector_index_single_write_real_kernels() {
    let directory = tempfile::tempdir().expect("temporary native store");
    let path = directory.path().join("native");
    let document = document_tower();
    let store = Store::create_native_graph(&path, native_options(), Some(document.clone()))
        .expect("fresh native graph");
    let coordinates = kernel_coordinates();
    let (node, observed, query_reads) = write_and_read_kernel(&store, &document)
        .expect("published vector source owns a native index");
    assert_eq!(observed.score_bits, 0x3f80_0002);
    assert_eq!(observed.distance_bits, 0.0_f64.to_bits());
    assert_eq!(observed.visited, 1);
    assert_eq!(query_reads.receipts.len(), 1);
    assert_eq!(query_reads.index_resolutions, 1);
    assert!(
        query_reads
            .receipts
            .iter()
            .all(|receipt| receipt.source_resolutions == 1 && receipt.index_resolutions == 1)
    );
    for rows in 2..=4 {
        let added = apply_repeated_vectors(
            &store,
            &document,
            &format!("small-{rows}"),
            rows,
            &coordinates,
        );
        assert_eq!(added.len(), rows);
    }
    let small = inspect_sources(&store, coordinates);
    for rows in 1..=4 {
        let source = small
            .iter()
            .find(|source| source.rows == rows)
            .expect("small indexed source");
        assert_eq!(source.seed_count, rows as usize);
        assert_eq!(source.live_rows, rows as usize);
    }
    store.close().expect("close native graph");
    drop(store);
    let reopened = Store::open_native_graph(&path, native_options(), Some(document))
        .expect("reopen native graph");
    let reopened_observed =
        read_kernel(&reopened, node).expect("reopened vector source owns the same native index");
    assert_eq!(reopened_observed, observed);
    reopened.close().expect("close reopened native graph");
}

fn inspect_sources(store: &Store, query: [f32; 2]) -> Vec<SourceReport> {
    inspect_sources_checked(store, query).expect("inspect vector sources")
}

fn inspect_retained_sources(
    store: &Store,
    lease: &NativeReadLease,
    query: [f32; 2],
) -> Vec<SourceReport> {
    let shared = GraphResources::from_store(store).expect("retained shared resources");
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).expect("retained query memory");
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime = RuntimeContext::new(lease, &control, &memory, RuntimeLimits::default())
        .expect("retained runtime");
    let capability = NativeReadCapability::admit(lease, &runtime).expect("retained capability");
    let mut resources = TreeResources::for_query(&mut runtime).expect("retained resources");
    let source = NativeQuerySource::new(capability, &resources, 32).expect("retained source");
    let catalog = NativeCatalog::open(&source, &mut resources).expect("retained catalog");
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).expect("retained view");
    InspectVectorSources { query }
        .consume(&view, &mut runtime)
        .expect("inspect retained vector sources")
}

struct InspectSourceFormats;

impl
    NativeReadConsumer<
        Vec<(
            super::super::Modality,
            super::super::codec::SourceFormat,
            bool,
        )>,
    > for InspectSourceFormats
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Vec<(
            super::super::Modality,
            super::super::codec::SourceFormat,
            bool,
        )>,
        TreeError,
    > {
        let sparse = view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let mut observed = Vec::new();
        for modality in [super::super::Modality::Text, super::super::Modality::Vector] {
            let mut sources = sparse.sources(modality, &mut resources)?;
            while let Some(source) = sources.next(&mut resources)? {
                observed.push((
                    modality,
                    source.format_for_test(),
                    source.has_vector_index_for_test(),
                ));
            }
        }
        Ok(observed)
    }
}

fn read_image_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .expect("index u64 extent")
            .try_into()
            .expect("index u64 width"),
    )
}

fn write_image(bytes: &mut [u8], offset: usize, value: &[u8]) {
    bytes
        .get_mut(offset..offset + value.len())
        .expect("index mutation extent")
        .copy_from_slice(value);
}

fn repair_graph_checksum(image: &mut [u8], graph_offset: usize, graph_length: usize) {
    let trailer = graph_offset + graph_length - crate::graph::block::NODE_BLOCK_TRAILER_LEN;
    let checksum = xxh3_64(
        image
            .get(graph_offset..trailer + 32)
            .expect("graph checksum extent"),
    );
    write_image(image, trailer + 32, &checksum.to_le_bytes());
}

struct AssertStructuralAndProfileRejection {
    rows: u32,
}

impl NativeReadConsumer<()> for AssertStructuralAndProfileRejection {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        use crate::graph::GraphParams;
        use crate::graph::build::{BuildMemoryEvent, NativeGraphBuildError, build_native_graph};

        let sparse = view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let mut sources = sparse.sources(super::super::Modality::Vector, &mut resources)?;
        while let Some(source) = sources.next(&mut resources)? {
            if source.row_count() != self.rows {
                continue;
            }
            let index = source
                .vector_index(&mut resources)?
                .ok_or(TreeError::Invalid("missing structural index"))?;
            source.validate_physical_row_rewrite_for_test(&index, &mut resources)?;
            let manifest = source.manifest_for_test();
            assert_eq!(manifest.format, super::super::codec::SourceFormat::V2);
            assert_eq!(manifest.vector_index, Some(index.payload_reference()));
            assert_eq!(manifest.row_table.reference().version, 1);
            assert_eq!(index.payload_reference().reference().version, 1);
            let mut encoded_manifest = [0_u8; super::super::codec::SOURCE_V2_BYTES];
            manifest.encode(&mut encoded_manifest)?;
            let decoded_manifest = super::super::codec::SourceManifest::decode(&encoded_manifest)?;
            assert_eq!(decoded_manifest.row_table, manifest.row_table);
            assert_eq!(decoded_manifest.vector_index, manifest.vector_index);
            let graph = index.graph()?;
            let graph_offset = usize::try_from(read_image_u64(index.encoded_bytes(), 224))
                .map_err(|_| TreeError::Memory)?;
            let graph_length = usize::try_from(read_image_u64(index.encoded_bytes(), 232))
                .map_err(|_| TreeError::Memory)?;
            let code_bytes = graph.layout().code_bytes();
            source.open_vector_index_image_for_test(index.encoded_bytes(), &mut resources)?;

            let mut image_version = index.encoded_bytes().to_vec();
            write_image(&mut image_version, 8, &2_u16.to_le_bytes());
            assert!(matches!(
                source.open_vector_index_image_for_test(&image_version, &mut resources),
                Err(TreeError::Invalid("native vector index header"))
            ));

            let mut discontiguous = index.encoded_bytes().to_vec();
            write_image(&mut discontiguous, 160, &257_u64.to_le_bytes());
            assert!(matches!(
                source.open_vector_index_image_for_test(&discontiguous, &mut resources),
                Err(TreeError::Invalid("native vector section order"))
            ));
            let mut wrong_length = index.encoded_bytes().to_vec();
            let identities_length = read_image_u64(&wrong_length, 168);
            write_image(
                &mut wrong_length,
                168,
                &identities_length.saturating_add(1).to_le_bytes(),
            );
            assert!(matches!(
                source.open_vector_index_image_for_test(&wrong_length, &mut resources),
                Err(TreeError::Invalid("native vector section order"))
            ));
            let mut trailing = index.encoded_bytes().to_vec();
            trailing.push(0);
            assert!(matches!(
                source.open_vector_index_image_for_test(&trailing, &mut resources),
                Err(TreeError::Invalid("native vector section geometry"))
            ));

            let mut seed_count = index.encoded_bytes().to_vec();
            write_image(&mut seed_count, 15, &[5]);
            assert!(matches!(
                source.open_vector_index_image_for_test(&seed_count, &mut resources),
                Err(TreeError::Invalid("native vector seed geometry"))
            ));
            let mut interpretation = index.encoded_bytes().to_vec();
            write_image(
                &mut interpretation,
                56,
                &(super::NATIVE_BUILD_SEED ^ 1).to_le_bytes(),
            );
            assert!(matches!(
                source.open_vector_index_image_for_test(&interpretation, &mut resources),
                Err(TreeError::Invalid("native vector interpretation"))
            ));

            let codes_offset = usize::try_from(read_image_u64(index.encoded_bytes(), 176))
                .map_err(|_| TreeError::Memory)?;
            let mut code_disagreement = index.encoded_bytes().to_vec();
            let changed_code = code_disagreement
                .get(codes_offset)
                .copied()
                .ok_or(TreeError::Invalid("code disagreement fixture"))?
                ^ 0x10;
            write_image(&mut code_disagreement, codes_offset, &[changed_code]);
            assert!(matches!(
                source.open_vector_index_image_for_test(&code_disagreement, &mut resources),
                Err(TreeError::Invalid("native vector code/graph disagreement"))
            ));

            let factors_offset = usize::try_from(read_image_u64(index.encoded_bytes(), 192))
                .map_err(|_| TreeError::Memory)?;
            let mut factor_disagreement = index.encoded_bytes().to_vec();
            let changed_factor = factor_disagreement
                .get(factors_offset)
                .copied()
                .ok_or(TreeError::Invalid("factor disagreement fixture"))?
                ^ 1;
            write_image(&mut factor_disagreement, factors_offset, &[changed_factor]);
            assert!(matches!(
                source.open_vector_index_image_for_test(&factor_disagreement, &mut resources),
                Err(TreeError::Invalid(
                    "native vector factor/graph disagreement"
                ))
            ));

            let identities_offset = usize::try_from(read_image_u64(index.encoded_bytes(), 160))
                .map_err(|_| TreeError::Memory)?;
            let mut swapped_identity = index.encoded_bytes().to_vec();
            let first_identity = swapped_identity
                .get(identities_offset..identities_offset + 24)
                .ok_or(TreeError::Invalid("first identity fixture"))?
                .to_vec();
            let second_identity = swapped_identity
                .get(identities_offset + 24..identities_offset + 48)
                .ok_or(TreeError::Invalid("second identity fixture"))?
                .to_vec();
            write_image(&mut swapped_identity, identities_offset, &second_identity);
            write_image(
                &mut swapped_identity,
                identities_offset + 24,
                &first_identity,
            );
            assert!(matches!(
                source.open_vector_index_image_for_test(&swapped_identity, &mut resources),
                Err(TreeError::Invalid("native vector/source identity"))
            ));

            let rescore_offset = usize::try_from(read_image_u64(index.encoded_bytes(), 208))
                .map_err(|_| TreeError::Memory)?;
            let mut changed_coordinate = index.encoded_bytes().to_vec();
            let original = f32::from_le_bytes(
                changed_coordinate
                    .get(rescore_offset..rescore_offset + 4)
                    .ok_or(TreeError::Invalid("coordinate fixture"))?
                    .try_into()
                    .map_err(|_| TreeError::Invalid("coordinate fixture width"))?,
            );
            write_image(
                &mut changed_coordinate,
                rescore_offset,
                &(original + 0.125).to_bits().to_le_bytes(),
            );
            assert!(matches!(
                source.open_vector_index_image_for_test(&changed_coordinate, &mut resources),
                Err(TreeError::Invalid("native vector/source coordinate"))
            ));
            let (self_row, duplicate_row) =
                (0..graph.node_count()).fold((None, None), |(self_row, duplicate_row), row| {
                    let degree = graph
                        .block(row)
                        .ok()
                        .map(|block| block.degree())
                        .unwrap_or(0);
                    (
                        self_row.or((degree >= 1).then_some(row)),
                        duplicate_row.or((degree >= 2).then_some(row)),
                    )
                });
            let self_row = self_row.ok_or(TreeError::Invalid("missing structural edge"))?;
            let duplicate_row =
                duplicate_row.ok_or(TreeError::Invalid("missing duplicate edge fixture"))?;

            let mut self_neighbor = index.encoded_bytes().to_vec();
            let self_offset = graph_offset
                + usize::try_from(
                    graph
                        .layout()
                        .block_offset(self_row)
                        .map_err(|_| TreeError::Invalid("self-neighbor block offset"))?,
                )
                .map_err(|_| TreeError::Memory)?
                + code_bytes
                + 16;
            write_image(&mut self_neighbor, self_offset, &self_row.to_le_bytes());
            repair_graph_checksum(&mut self_neighbor, graph_offset, graph_length);
            assert!(matches!(
                source.open_vector_index_image_for_test(&self_neighbor, &mut resources),
                Err(TreeError::Invalid("native vector graph"))
            ));

            let mut out_of_range = index.encoded_bytes().to_vec();
            write_image(&mut out_of_range, self_offset, &self.rows.to_le_bytes());
            repair_graph_checksum(&mut out_of_range, graph_offset, graph_length);
            assert!(matches!(
                source.open_vector_index_image_for_test(&out_of_range, &mut resources),
                Err(TreeError::Invalid("native vector graph"))
            ));

            let mut seed_flag = index.encoded_bytes().to_vec();
            let flag_offset = graph_offset
                + usize::try_from(
                    graph
                        .layout()
                        .block_offset(0)
                        .map_err(|_| TreeError::Invalid("seed flag block offset"))?,
                )
                .map_err(|_| TreeError::Memory)?
                + code_bytes
                + 13;
            let changed_flag = seed_flag
                .get(flag_offset)
                .copied()
                .ok_or(TreeError::Invalid("seed flag fixture"))?
                ^ 1;
            write_image(&mut seed_flag, flag_offset, &[changed_flag]);
            repair_graph_checksum(&mut seed_flag, graph_offset, graph_length);
            assert!(matches!(
                source.open_vector_index_image_for_test(&seed_flag, &mut resources),
                Err(TreeError::Invalid("native vector seed/graph disagreement"))
            ));

            let mut duplicate_neighbor = index.encoded_bytes().to_vec();
            let duplicate_offset = graph_offset
                + usize::try_from(
                    graph
                        .layout()
                        .block_offset(duplicate_row)
                        .map_err(|_| TreeError::Invalid("duplicate-neighbor block offset"))?,
                )
                .map_err(|_| TreeError::Memory)?
                + code_bytes
                + 16;
            let first = duplicate_neighbor
                .get(duplicate_offset..duplicate_offset + 4)
                .ok_or(TreeError::Invalid("duplicate fixture first edge"))?
                .to_vec();
            write_image(&mut duplicate_neighbor, duplicate_offset + 4, &first);
            repair_graph_checksum(&mut duplicate_neighbor, graph_offset, graph_length);
            assert!(matches!(
                source.open_vector_index_image_for_test(&duplicate_neighbor, &mut resources),
                Err(TreeError::Invalid("native vector graph"))
            ));

            let mut codes = Vec::new();
            let mut factors = Vec::new();
            for row in 0..index.row_count() {
                codes.extend_from_slice(index.code(row)?);
                factors.push(index.factors(row)?);
            }
            let alternate = build_native_graph(
                index.dimensions() as usize,
                &codes,
                &factors,
                index.rescore(),
                GraphParams::angular(),
                super::NATIVE_BUILD_SEED,
                &mut |_| Ok::<_, TreeError>(()),
                &mut |event| match event {
                    BuildMemoryEvent::Acquire(_)
                    | BuildMemoryEvent::Reconcile { .. }
                    | BuildMemoryEvent::Release(_) => Ok::<_, TreeError>(()),
                },
            )
            .map_err(|error| match error {
                NativeGraphBuildError::Control(error) => error,
                NativeGraphBuildError::Build(_) => {
                    TreeError::Invalid("alternate profile graph build")
                }
            })?;
            let params = GraphParams::angular();
            let mut profile = index
                .encoded_bytes()
                .get(..graph_offset)
                .ok_or(TreeError::Invalid("profile graph prefix"))?
                .to_vec();
            profile.extend_from_slice(alternate.encoded_region());
            write_image(&mut profile, 14, &[2]);
            write_image(&mut profile, 44, &[params.r_target(), params.r_max()]);
            write_image(&mut profile, 46, &params.l_build().to_le_bytes());
            write_image(
                &mut profile,
                48,
                &params.alpha_build().to_bits().to_le_bytes(),
            );
            write_image(
                &mut profile,
                52,
                &params.alpha_refine().to_bits().to_le_bytes(),
            );
            write_image(
                &mut profile,
                232,
                &(alternate.encoded_region().len() as u64).to_le_bytes(),
            );
            let first_seed = *alternate
                .entry_points()
                .first()
                .ok_or(TreeError::Invalid("alternate profile seed"))?;
            for slot in 0..4 {
                let seed = alternate
                    .entry_points()
                    .get(slot)
                    .copied()
                    .unwrap_or(first_seed);
                write_image(&mut profile, 240 + slot * 4, &seed.to_le_bytes());
            }
            assert!(matches!(
                source.open_vector_index_image_for_test(&profile, &mut resources),
                Err(TreeError::Invalid("native vector profile interpretation"))
            ));
            return Ok(());
        }
        Err(TreeError::Invalid("missing structural source"))
    }
}

struct AssertAngularNormRejection {
    rows: u32,
}

impl NativeReadConsumer<()> for AssertAngularNormRejection {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let sparse = view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let mut sources = sparse.sources(super::super::Modality::Vector, &mut resources)?;
        while let Some(source) = sources.next(&mut resources)? {
            if source.row_count() != self.rows {
                continue;
            }
            let index = source
                .vector_index(&mut resources)?
                .ok_or(TreeError::Invalid("missing angular index"))?;
            let rescore_offset = usize::try_from(read_image_u64(index.encoded_bytes(), 208))
                .map_err(|_| TreeError::Memory)?;
            let mut non_unit = index.encoded_bytes().to_vec();
            write_image(&mut non_unit, rescore_offset, &2.0_f32.to_le_bytes());
            assert!(matches!(
                source.open_vector_index_image_for_test(&non_unit, &mut resources),
                Err(TreeError::Invalid("native angular vector norm"))
            ));
            return Ok(());
        }
        Err(TreeError::Invalid("missing angular source"))
    }
}

#[test]
fn native_vector_index_small_writes_and_source_isolation() {
    let directory = tempfile::tempdir().expect("temporary native store");
    let path = directory.path().join("native");
    let document = document_tower();
    let store = Store::create_native_graph(&path, native_options(), Some(document.clone()))
        .expect("fresh native graph");
    let coordinates = [0.25_f32, -0.5_f32];
    let fixture = prepare_small_writes_fixture(&store, &document, 0x158)
        .expect("shared small-writes fixture");
    let separate = fixture.separate;
    let before = fixture.before;
    assert_eq!(fixture.report.prepared_images_per_apply, [1; 5]);
    assert_eq!(fixture.report.preparation_index_resolutions, [0; 5]);
    assert_eq!(fixture.report.query_reads.receipts.len(), 5);
    assert_eq!(fixture.report.query_reads.index_resolutions, 5);
    assert!(
        fixture
            .report
            .query_reads
            .receipts
            .iter()
            .all(|receipt| { receipt.source_resolutions == 1 && receipt.index_resolutions == 1 })
    );
    assert_eq!(fixture.report.query_prepare_events, 0);
    let retained = store
        .admit_native_read()
        .expect("retain pre-mutation admission");
    let unaffected_fingerprints = before
        .iter()
        .filter(|source| {
            !source
                .identities
                .iter()
                .any(|identity| identity.0 == separate[0] || identity.0 == separate[1])
        })
        .map(|source| source.fingerprint)
        .collect::<Vec<_>>();

    let replacement = [0.75_f32, -0.25_f32];
    assert_eq!(
        apply_vector(
            &store,
            &document,
            "separate-0",
            2,
            StructuredOperation::Put(EntityId::Node(separate[0])),
            &replacement,
        ),
        separate[0]
    );
    let delete = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "separate-1").expect("delete key"),
        revision: GraphRevision::new(2).expect("delete revision"),
        operation: StructuredOperation::Delete(
            EntityId::Node(separate[1]),
            GraphDeleteMode::Restrict,
        ),
        image: None,
    }];
    store
        .apply_native_graph(&delete, &QueryControl::Cancel(CancelToken::new()))
        .expect("delete vector node");
    let after = inspect_sources(&store, replacement);
    let retained_after = inspect_retained_sources(&store, &retained, coordinates);
    assert_eq!(
        retained_after
            .iter()
            .map(|source| (source.fingerprint, source.identities.as_slice()))
            .collect::<Vec<_>>(),
        before
            .iter()
            .map(|source| (source.fingerprint, source.identities.as_slice()))
            .collect::<Vec<_>>()
    );
    for fingerprint in unaffected_fingerprints {
        assert!(after.iter().any(|source| source.fingerprint == fingerprint));
    }
    assert!(after.iter().any(|source| {
        source
            .identities
            .iter()
            .any(|identity| *identity == (separate[0], 2))
    }));
    assert!(after.iter().all(|source| {
        source
            .live_identities
            .iter()
            .all(|identity| identity.0 != separate[1])
    }));
    drop(retained);
    store.close().expect("close native graph");
}

fn payload_fixture(role: BlockKind, artifact: u128, length: u64) -> PayloadRef {
    PayloadRef::new(
        role,
        length,
        PhysicalRef {
            artifact: ArtifactId::new(artifact).expect("artifact"),
            offset: 96,
            length: u32::try_from(length).expect("fixture length"),
            kind: role,
            version: 1,
        },
    )
    .expect("payload fixture")
}

#[test]
fn native_vector_index_identity_space_and_geometry() {
    use super::super::codec::{SOURCE_V1_BYTES, SOURCE_V2_BYTES, SourceFormat, SourceManifest};

    let identity = super::test_support::run_identity_probe(0x158);
    assert_eq!(
        identity.write_identities,
        [(1_u128 << 80) - 1, 1_u128 << 80, (1_u128 << 80) + 1]
    );
    let document = document_tower();

    let row_table = payload_fixture(BlockKind::RetrievalRows, 9001, 80);
    let index = payload_fixture(BlockKind::RetrievalVectorIndex, 9002, 256);
    let legacy = SourceManifest {
        format: SourceFormat::V1,
        modality: super::super::Modality::Vector,
        generation: crate::property_graph::GraphGeneration::new(7),
        sequence: 9,
        rows: 1,
        row_table,
        lexical: None,
        vector_index: None,
    };
    let mut v1 = [0_u8; SOURCE_V1_BYTES];
    legacy.encode(&mut v1).expect("encode legacy source");
    let decoded = SourceManifest::decode(&v1).expect("decode legacy source");
    let mut v1_roundtrip = [0_u8; SOURCE_V1_BYTES];
    decoded
        .encode(&mut v1_roundtrip)
        .expect("re-encode legacy source");
    assert_eq!(v1_roundtrip, v1);
    assert_eq!(u16::from_le_bytes([v1[6], v1[7]]), 1);
    assert_eq!(v1.len(), 144);
    assert_eq!(decoded.vector_index, None);
    assert_eq!(row_table.reference().version, 1);
    let missing_index = SourceManifest {
        format: SourceFormat::V2,
        ..legacy
    };
    let mut v2 = [0_u8; SOURCE_V2_BYTES];
    assert!(missing_index.encode(&mut v2).is_err());
    let indexed = SourceManifest {
        format: SourceFormat::V2,
        vector_index: Some(index),
        ..legacy
    };
    indexed
        .encode(&mut v2)
        .expect("encode indexed vector source");
    assert_eq!(u16::from_le_bytes([v2[6], v2[7]]), 2);
    assert_eq!(v2.len(), 200);
    let decoded_v2 = SourceManifest::decode(&v2).expect("decode indexed vector source");
    assert_eq!(decoded_v2.row_table, row_table);
    assert_eq!(decoded_v2.vector_index, Some(index));
    assert_eq!(index.reference().version, 1);

    let geometry_directory = tempfile::tempdir().expect("temporary geometry store");
    let geometry = Store::create_native_graph(
        geometry_directory.path().join("native"),
        native_options(),
        Some(document.clone()),
    )
    .expect("fresh geometry graph");
    let mixed_coordinates = [1.0_f32, 0.0_f32];
    let mixed_embedding =
        CanonicalEmbedding::new(&document, &mixed_coordinates).expect("mixed embedding");
    let mixed = CanonicalContents::node(
        &mut [],
        &mut [],
        Some("mixed text and vector"),
        Some(mixed_embedding),
    )
    .expect("mixed node");
    geometry
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "mixed").expect("mixed key"),
                revision: GraphRevision::new(1).expect("mixed revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&mixed)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("mixed text/vector write");
    let _ = apply_repeated_vectors(&geometry, &document, "geometry", 8, &mixed_coordinates);
    geometry
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            32,
            AssertStructuralAndProfileRejection { rows: 8 },
        )
        .expect("reject structural and profile mutations");
    let formats = geometry
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            InspectSourceFormats,
        )
        .expect("inspect actual source formats");
    assert!(formats.iter().any(|entry| {
        *entry
            == (
                super::super::Modality::Text,
                super::super::codec::SourceFormat::V2,
                false,
            )
    }));
    assert!(formats.iter().any(|entry| {
        *entry
            == (
                super::super::Modality::Vector,
                super::super::codec::SourceFormat::V2,
                true,
            )
    }));
    geometry.close().expect("close geometry graph");

    let no_document_directory = tempfile::tempdir().expect("temporary no-document store");
    let no_document = Store::create_native_graph(
        no_document_directory.path().join("native"),
        native_options(),
        None,
    )
    .expect("fresh no-document graph");
    let text_only =
        CanonicalContents::node(&mut [], &mut [], Some("text only"), None).expect("text-only node");
    no_document
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "text-only")
                    .expect("text-only key"),
                revision: GraphRevision::new(1).expect("text-only revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&text_only)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("no-document text write");
    let no_document_formats = no_document
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            InspectSourceFormats,
        )
        .expect("inspect no-document sources");
    assert_eq!(
        no_document_formats,
        [(
            super::super::Modality::Text,
            super::super::codec::SourceFormat::V2,
            false,
        )]
    );
    no_document.close().expect("close no-document graph");

    let angular_directory = tempfile::tempdir().expect("temporary angular store");
    let mut angular_document = document_tower();
    angular_document.normalization = Normalization::L2;
    let angular = Store::create_native_graph(
        angular_directory.path().join("native"),
        native_options(),
        Some(angular_document.clone()),
    )
    .expect("fresh angular graph");
    let _ = apply_repeated_vectors(&angular, &angular_document, "unit", 4, &[1.0_f32, 0.0_f32]);
    let angular_before = angular
        .admit_native_read()
        .expect("angular before invalid write");
    let angular_state = (
        angular_before.bundle().base(),
        angular_before.bundle().sequence(),
    );
    drop(angular_before);
    let invalid_coordinates = [2.0_f32, 0.0_f32];
    let invalid_embedding = CanonicalEmbedding::new(&angular_document, &invalid_coordinates)
        .expect("invalid angular embedding remains lossless");
    let invalid_node = CanonicalContents::node(&mut [], &mut [], None, Some(invalid_embedding))
        .expect("invalid angular node");
    let invalid = angular.apply_native_graph(
        &[StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "non-unit").expect("non-unit key"),
            revision: GraphRevision::new(1).expect("non-unit revision"),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&invalid_node)),
        }],
        &QueryControl::Cancel(CancelToken::new()),
    );
    assert!(matches!(
        invalid,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            TreeError::Invalid("native angular vector norm")
        ))
    ));
    let angular_after = angular
        .admit_native_read()
        .expect("angular after invalid write");
    assert_eq!(
        (
            angular_after.bundle().base(),
            angular_after.bundle().sequence()
        ),
        angular_state
    );
    drop(angular_after);
    angular
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            32,
            AssertAngularNormRejection { rows: 4 },
        )
        .expect("reject non-unit angular rescore row");
    angular.close().expect("close angular graph");
}

#[test]
fn native_vector_index_quantizer_builder_reuse() {
    use crate::graph::GraphParams;
    use crate::graph::build::{
        BuildMemoryEvent, GraphBuildPasses, build_graph_for_test, build_native_graph,
    };
    use crate::lifecycle::durability::DurabilityPolicy;
    use crate::meta::{AliveSet, ColumnStoreBuilder, Schema};
    use crate::quant::quantize_bit4;
    use crate::segment::SegmentId;
    use crate::segment::reader::SegmentReader;
    use crate::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
    use crate::vfs::StdVfs;

    struct ReuseObservation {
        codes: Vec<u8>,
        factor_bits: Vec<[u32; 3]>,
        rescore_bits: Vec<u32>,
        graph_bytes: Vec<u8>,
        entry_points: Vec<u32>,
        identities: Vec<(NodeId, u64)>,
        candidates: Vec<(u32, NodeId, u64)>,
    }

    struct InspectReuseIndex {
        query: [f32; 2],
    }

    impl NativeReadConsumer<ReuseObservation> for InspectReuseIndex {
        fn consume<'s, 'lease, 'm, 'g>(
            &mut self,
            view: &GraphReadView<'s, 'lease, 'm, 'g>,
            runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<ReuseObservation, TreeError> {
            let sparse = view.sparse_view(runtime)?;
            let mut resources = TreeResources::for_query(runtime)?;
            let mut sources = sparse.sources(super::super::Modality::Vector, &mut resources)?;
            let source = sources
                .next(&mut resources)?
                .ok_or(TreeError::Invalid("reuse vector source missing"))?;
            let index = source
                .vector_index(&mut resources)?
                .ok_or(TreeError::Invalid("reuse native index missing"))?;
            if sources.next(&mut resources)?.is_some() {
                return Err(TreeError::Invalid("reuse has multiple vector sources"));
            }
            let graph = index.graph()?;
            let mut codes = Vec::new();
            let mut factor_bits = Vec::new();
            let mut identities = Vec::new();
            for row in 0..index.row_count() {
                codes.extend_from_slice(index.code(row)?);
                factor_bits.push(index.factors(row)?.persisted_fields().map(f32::to_bits));
                identities.push(index.identity(row)?);
            }
            let graph_end = index
                .graph_offset
                .checked_add(index.graph_length)
                .ok_or(TreeError::Memory)?;
            let graph_bytes = index
                .encoded_bytes()
                .get(index.graph_offset..graph_end)
                .ok_or(TreeError::Invalid("reuse graph extent"))?
                .to_vec();
            let mut scratch =
                GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                    .map_err(|_| TreeError::Invalid("reuse graph scratch"))?;
            let mut searcher = GraphSearcher::new(graph, index.rescore(), &mut scratch)
                .map_err(|_| TreeError::Invalid("reuse graph binding"))?;
            let result = searcher
                .search(
                    GraphSearchRequest::new(&self.query, index.row_count() as usize, 0x158e)
                        .with_ef(index.row_count() as usize),
                    None,
                )
                .map_err(|_| TreeError::Invalid("reuse graph search"))?;
            let mut candidates = Vec::new();
            for candidate in result.candidates() {
                let (node, _) = index.identity(candidate.row_id())?;
                candidates.push((candidate.row_id(), node, candidate.distance().to_bits()));
            }
            Ok(ReuseObservation {
                codes,
                factor_bits,
                rescore_bits: index
                    .rescore()
                    .iter()
                    .map(|value| value.to_bits())
                    .collect(),
                graph_bytes,
                entry_points: index.seed_row_ids().to_vec(),
                identities,
                candidates,
            })
        }
    }

    fn apply_cohort(store: &Store, document: &EmbeddingTower, cohort: &[[f32; 2]]) -> Vec<NodeId> {
        let embeddings = cohort
            .iter()
            .map(|row| CanonicalEmbedding::new(document, row).expect("reuse embedding"))
            .collect::<Vec<_>>();
        let mut labels = (0..cohort.len())
            .map(|_| Vec::<GraphName<'_>>::new())
            .collect::<Vec<_>>();
        let mut properties = (0..cohort.len())
            .map(|_| Vec::new())
            .collect::<Vec<Vec<crate::property_graph::GraphProperty<'_>>>>();
        let images = labels
            .iter_mut()
            .zip(properties.iter_mut())
            .zip(embeddings)
            .map(|((labels, properties), embedding)| {
                CanonicalContents::node(labels, properties, None, Some(embedding))
                    .expect("reuse node")
            })
            .collect::<Vec<_>>();
        let keys = (0..cohort.len())
            .map(|index| format!("reuse-{index}"))
            .collect::<Vec<_>>();
        let requests = images
            .iter()
            .zip(&keys)
            .map(|(image, key)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", key).expect("reuse key"),
                revision: GraphRevision::new(1).expect("reuse revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(image)),
            })
            .collect::<Vec<_>>();
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("ordinary native reuse write")
            .iter()
            .map(|receipt| match receipt.entity {
                EntityId::Node(node) => node,
                EntityId::Relationship(_) => panic!("reuse receipt changed identity domain"),
            })
            .collect()
    }

    let directory = tempfile::tempdir().expect("temporary reuse store");
    let document = document_tower();
    let store = Store::create_native_graph(
        directory.path().join("native"),
        native_options(),
        Some(document.clone()),
    )
    .expect("fresh reuse graph");
    let cohort = [
        [0.0_f32, 0.0_f32],
        [1.0, 0.0],
        [0.0, 1.0],
        [-1.0, 0.0],
        [0.0, -1.0],
        [0.5, 0.5],
        [-0.5, 0.5],
        [-0.5, -0.5],
    ];
    let coordinates = cohort.into_iter().flatten().collect::<Vec<_>>();
    let nodes = apply_cohort(&store, &document, &cohort);
    let query = [0.25_f32, 0.5_f32];
    let native = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            InspectReuseIndex { query },
        )
        .expect("inspect ordinary native reuse index");
    assert_eq!(
        native.identities,
        nodes
            .iter()
            .copied()
            .map(|node| (node, 1))
            .collect::<Vec<_>>()
    );

    let mut codes = vec![0_u8; cohort.len()];
    let mut factors = Vec::new();
    for (row, output) in coordinates.chunks_exact(2).zip(codes.iter_mut()) {
        factors
            .push(quantize_bit4(row, std::slice::from_mut(output)).expect("public quantization"));
    }
    let segment_directory = tempfile::tempdir().expect("legacy reuse segment directory");
    let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("empty schema"));
    for row in 0..cohort.len() {
        columns
            .push_row(row as i64, &[])
            .expect("reuse segment row");
    }
    let columns = columns.finish().expect("reuse segment columns");
    let alive = AliveSet::new(cohort.len() as u32);
    let segment_id = SegmentId::new(158, [0x5e; 10]);
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
        .expect("derived reuse policy");
    write_segment(
        &StdVfs,
        segment_directory.path(),
        SegmentBuild {
            id: segment_id,
            scheme: 4,
            dims: 2,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &coordinates,
            columns: &columns,
            alive: &alive,
        },
        policy,
    )
    .expect("write legacy reuse segment");
    let reader = SegmentReader::open(
        &StdVfs,
        &segment_directory.path().join(segment_id.file_name()),
        segment_id,
    )
    .expect("open legacy reuse segment");
    let legacy = build_graph_for_test(
        &reader,
        GraphParams::sift_1m(),
        super::NATIVE_BUILD_SEED,
        GraphBuildPasses::One,
    )
    .expect("legacy one-pass reuse graph");
    assert_eq!(native.codes, reader.bit4_codes().expect("legacy codes"));
    assert_eq!(
        native.factor_bits,
        reader
            .bit4_factors()
            .expect("legacy factors")
            .iter()
            .map(|factor| factor.persisted_fields().map(f32::to_bits))
            .collect::<Vec<_>>()
    );
    assert_eq!(native.graph_bytes, legacy.encoded_region());
    assert_eq!(native.entry_points, legacy.entry_points());

    assert_eq!(
        native.rescore_bits,
        coordinates
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    );
    let literals = cohort
        .iter()
        .map(|row| {
            row.iter()
                .zip(query)
                .map(|(value, query)| {
                    let difference = f64::from(*value) - f64::from(query);
                    difference * difference
                })
                .sum::<f64>()
        })
        .collect::<Vec<_>>();
    assert_eq!(native.candidates.len(), cohort.len());
    let mut returned = vec![false; cohort.len()];
    for (row, node, distance) in &native.candidates {
        let row = *row as usize;
        assert!(row < cohort.len());
        assert!(!returned[row]);
        returned[row] = true;
        assert_eq!(*node, nodes[row]);
        assert_eq!(*distance, literals[row].to_bits());
    }
    assert!(returned.into_iter().all(|seen| seen));

    let decoded = crate::graph::block::decode_node_blocks(&native.graph_bytes)
        .expect("native reuse graph decodes");
    let mut reachable = vec![false; cohort.len()];
    let mut pending = native.entry_points.clone();
    while let Some(row) = pending.pop() {
        let row_index = row as usize;
        assert!(row_index < cohort.len());
        if reachable[row_index] {
            continue;
        }
        reachable[row_index] = true;
        let block = decoded.block(row).expect("reuse graph row");
        let mut neighbors = block
            .neighbors_padded()
            .take(usize::from(block.degree()))
            .collect::<Vec<_>>();
        assert!(
            neighbors
                .iter()
                .all(|neighbor| *neighbor < decoded.node_count())
        );
        let original_len = neighbors.len();
        neighbors.sort_unstable();
        neighbors.dedup();
        assert_eq!(neighbors.len(), original_len);
        pending.extend(neighbors);
    }
    assert!(reachable.into_iter().all(|seen| seen));

    let build = |work: &mut u64, events: &mut Vec<BuildMemoryEvent>| match build_native_graph(
        2,
        &codes,
        &factors,
        &coordinates,
        GraphParams::sift_1m(),
        super::NATIVE_BUILD_SEED,
        &mut |units| {
            *work += units;
            Ok::<_, ()>(())
        },
        &mut |event| {
            events.push(event);
            Ok::<_, ()>(())
        },
    ) {
        Ok(value) => value,
        Err(_) => panic!("controlled Vamana build"),
    };
    let mut first_work = 0;
    let mut first_events = Vec::new();
    let first = build(&mut first_work, &mut first_events);
    let mut second_work = 0;
    let mut second_events = Vec::new();
    let second = build(&mut second_work, &mut second_events);
    assert_eq!(first.encoded_region(), second.encoded_region());
    assert_eq!(first.entry_points(), second.entry_points());
    assert_eq!(first_work, second_work);
    assert_eq!(first_events.len(), second_events.len());
    store.close().expect("close reuse graph");
}

#[test]
fn native_vector_index_prepare_limits_controls_release() {
    use crate::graph::GraphParams;
    use crate::graph::block::{GraphNodeError, decode_node_blocks_controlled};
    use crate::graph::build::{BuildMemoryEvent, NativeGraphBuildError, build_native_graph};
    use crate::quant::{
        Bit4ControlError, Bit4Scratch, est_dot_bit4, est_dot_bit4_controlled,
        prepare_bit4_query_controlled, quantize_bit4_controlled,
    };

    let validation_directory = tempfile::tempdir().expect("validation control store");
    let validation_store = Store::create_native_graph(
        validation_directory.path().join("native"),
        native_options(),
        Some(document_tower()),
    )
    .expect("validation resource owner");
    let validation_shared =
        GraphResources::from_store(&validation_store).expect("validation resources");
    let validation_baseline = validation_shared
        .reserved_bytes()
        .expect("validation baseline");
    let mut invalid_row = vec![1.0_f32; 513];
    invalid_row[512] = f32::NAN;
    let mut invalid_codes = vec![0x5a_u8; 513_usize.div_ceil(2)];
    let mut validation_scratch = Bit4Scratch::try_new(513).expect("validation scratch");
    let validation_cancel = CancelToken::new();
    let validation_control = QueryControl::Cancel(validation_cancel.clone());
    let mut quantize_callbacks = 0;
    let cancelled_validation = {
        let mut resources = TreeResources::new(&validation_control, &validation_shared, u64::MAX)
            .expect("quantizer validation resources");
        quantize_bit4_controlled(
            &invalid_row,
            &mut invalid_codes,
            &mut validation_scratch,
            &mut |units| {
                quantize_callbacks += 1;
                assert!(units <= 256);
                if quantize_callbacks == 2 {
                    validation_cancel.cancel();
                }
                resources.step(units)
            },
        )
    };
    let clock = std::sync::Arc::new(crate::lifecycle::ManualMonotonicClock::new());
    let validation_deadline = crate::lifecycle::Deadline::after_with_test_clock(
        std::time::Duration::from_secs(1),
        std::sync::Arc::clone(&clock) as std::sync::Arc<dyn crate::lifecycle::MonotonicClock>,
    )
    .expect("validation deadline");
    let deadline_control = QueryControl::Deadline(validation_deadline);
    let mut query_callbacks = 0;
    let expired_validation = {
        let mut resources = TreeResources::new(&deadline_control, &validation_shared, u64::MAX)
            .expect("query validation resources");
        prepare_bit4_query_controlled(&invalid_row, super::NATIVE_BUILD_SEED, &mut |units| {
            query_callbacks += 1;
            assert!(units <= 256);
            if query_callbacks == 2 {
                clock.advance(std::time::Duration::from_secs(2));
            }
            resources.step(units)
        })
    };
    assert_eq!(
        validation_shared
            .reserved_bytes()
            .expect("validation release"),
        validation_baseline
    );
    assert_eq!(
        (
            matches!(
                cancelled_validation,
                Err(Bit4ControlError::Control(TreeError::Control(
                    crate::lifecycle::QueryError::Cancelled { partial: false }
                )))
            ),
            quantize_callbacks,
            matches!(
                expired_validation,
                Err(Bit4ControlError::Control(TreeError::Control(
                    crate::lifecycle::QueryError::Timeout { partial: false }
                )))
            ),
            query_callbacks,
        ),
        (true, 2, true, 2),
        "real control must interrupt both validations before the late non-finite coordinate",
    );
    assert!(invalid_codes.iter().all(|code| *code == 0x5a));
    let clean_control = QueryControl::Cancel(CancelToken::new());
    let mut quantize_validation_chunks = Vec::new();
    let clean_quantization = {
        let mut resources = TreeResources::new(&clean_control, &validation_shared, u64::MAX)
            .expect("clean quantizer validation resources");
        quantize_bit4_controlled(
            &invalid_row,
            &mut invalid_codes,
            &mut validation_scratch,
            &mut |units| {
                quantize_validation_chunks.push(units);
                resources.step(units)
            },
        )
    };
    assert!(matches!(
        clean_quantization,
        Err(Bit4ControlError::Quant(
            crate::quant::QuantError::NonFinite { index: 512 }
        ))
    ));
    assert_eq!(quantize_validation_chunks, [256, 256, 1]);
    assert!(invalid_codes.iter().all(|code| *code == 0x5a));
    let mut query_validation_chunks = Vec::new();
    let clean_query = {
        let mut resources = TreeResources::new(&clean_control, &validation_shared, u64::MAX)
            .expect("clean query validation resources");
        prepare_bit4_query_controlled(&invalid_row, super::NATIVE_BUILD_SEED, &mut |units| {
            query_validation_chunks.push(units);
            resources.step(units)
        })
    };
    assert!(matches!(
        clean_query,
        Err(Bit4ControlError::Quant(
            crate::quant::QuantError::NonFinite { index: 512 }
        ))
    ));
    assert_eq!(query_validation_chunks, [256, 256, 1]);
    assert_eq!(
        validation_shared
            .reserved_bytes()
            .expect("clean validation release"),
        validation_baseline
    );
    validation_store.close().expect("close validation store");

    let odd_dimensions = 513_usize;
    let odd_row = (0..odd_dimensions)
        .map(|dimension| (dimension as f32 - 256.0) / 257.0)
        .collect::<Vec<_>>();
    let odd_query = odd_row.iter().rev().copied().collect::<Vec<_>>();
    let mut odd_codes = vec![0_u8; odd_dimensions.div_ceil(2)];
    let mut odd_scratch = Bit4Scratch::try_new(odd_dimensions).expect("odd Bit4 scratch");
    let odd_factors =
        match quantize_bit4_controlled(&odd_row, &mut odd_codes, &mut odd_scratch, &mut |_| {
            Ok::<_, ()>(())
        }) {
            Ok(value) => value,
            Err(_) => panic!("odd controlled quantizer"),
        };
    let (prepared_odd_query, _) =
        match prepare_bit4_query_controlled(&odd_query, super::NATIVE_BUILD_SEED, &mut |_| {
            Ok::<_, ()>(())
        }) {
            Ok(value) => value,
            Err(_) => panic!("odd controlled query"),
        };
    let compatibility_score = est_dot_bit4(&prepared_odd_query, &odd_codes, odd_factors)
        .expect("odd compatibility score");
    let mut score_chunks = Vec::new();
    let controlled_score =
        match est_dot_bit4_controlled(&prepared_odd_query, &odd_codes, odd_factors, &mut |units| {
            score_chunks.push(units);
            Ok::<_, ()>(())
        }) {
            Ok(value) => value,
            Err(_) => panic!("odd controlled score"),
        };
    assert_eq!(controlled_score.to_bits(), compatibility_score.to_bits());
    assert_eq!(score_chunks, [256, 256, 1]);
    let mut score_calls = 0_usize;
    let cancelled_score =
        est_dot_bit4_controlled(&prepared_odd_query, &odd_codes, odd_factors, &mut |_| {
            score_calls += 1;
            if score_calls == 2 {
                Err("observed controlled score")
            } else {
                Ok(())
            }
        });
    assert!(matches!(
        cancelled_score,
        Err(Bit4ControlError::Control("observed controlled score"))
    ));
    assert_eq!(score_calls, 2);

    let coordinates = (0..64)
        .flat_map(|row| [row as f32 / 64.0, 1.0 - row as f32 / 64.0])
        .collect::<Vec<_>>();
    let mut codes = vec![0_u8; 64];
    let mut factors = Vec::new();
    let mut scratch = Bit4Scratch::try_new(2).expect("limit Bit4 scratch");
    let mut quantizer_work = 0_u64;
    for (row, output) in coordinates.chunks_exact(2).zip(codes.iter_mut()) {
        let result = quantize_bit4_controlled(
            row,
            std::slice::from_mut(output),
            &mut scratch,
            &mut |units| {
                quantizer_work += units;
                Ok::<_, ()>(())
            },
        );
        factors.push(match result {
            Ok(value) => value,
            Err(_) => panic!("clean quantizer"),
        });
    }
    let quantizer_fire = quantizer_work / 2;
    let mut observed = 0_u64;
    let mut failed_output = [0_u8; 1];
    let mut failed_scratch = Bit4Scratch::try_new(2).expect("failed Bit4 scratch");
    let failed = quantize_bit4_controlled(
        &coordinates[..2],
        &mut failed_output,
        &mut failed_scratch,
        &mut |units| {
            observed += units;
            if observed >= quantizer_fire.min(2) {
                Err("observed quantizer control")
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(failed, Err(Bit4ControlError::Control(_))));
    assert!(observed > 0);

    let mut clean_work = 0_u64;
    let mut live = 0_usize;
    let mut acquired = None;
    let mut peak = 0_usize;
    let clean = build_native_graph(
        2,
        &codes,
        &factors,
        &coordinates,
        GraphParams::sift_1m(),
        super::NATIVE_BUILD_SEED,
        &mut |units| {
            clean_work += units;
            Ok::<_, &'static str>(())
        },
        &mut |event| {
            match event {
                BuildMemoryEvent::Acquire(bytes) => {
                    assert!(acquired.replace(bytes).is_none());
                    peak = peak.max(live.checked_add(bytes).expect("clean live bytes"));
                }
                BuildMemoryEvent::Reconcile { requested, actual } => {
                    assert_eq!(acquired.take(), Some(requested));
                    live = live.checked_add(actual).expect("clean actual bytes");
                    peak = peak.max(live);
                }
                BuildMemoryEvent::Release(bytes) => {
                    assert!(acquired.is_none());
                    live = live.checked_sub(bytes).expect("clean release bytes");
                }
            }
            Ok::<_, &'static str>(())
        },
    );
    let clean = match clean {
        Ok(value) => value,
        Err(_) => panic!("clean controlled graph"),
    };
    let mut decode_chunks = Vec::new();
    decode_node_blocks_controlled(clean.encoded_region(), &mut |units| {
        decode_chunks.push(units);
        true
    })
    .expect("controlled graph output validation");
    assert!(decode_chunks.iter().all(|units| *units <= 256));
    assert!(decode_chunks.contains(&256));
    let mut decode_calls = 0_usize;
    let cancelled_decode = decode_node_blocks_controlled(clean.encoded_region(), &mut |_| {
        decode_calls += 1;
        decode_calls != 2
    });
    assert!(matches!(cancelled_decode, Err(GraphNodeError::Control)));
    assert_eq!(decode_calls, 2);
    assert!(clean_work > 256);
    assert!(peak > live);
    assert_eq!(live, clean.resident_bytes().expect("retained graph bytes"));
    let fire_at = clean_work / 2;
    let mut failed_work = 0_u64;
    let failed = build_native_graph(
        2,
        &codes,
        &factors,
        &coordinates,
        GraphParams::sift_1m(),
        super::NATIVE_BUILD_SEED,
        &mut |units| {
            failed_work += units;
            if failed_work >= fire_at {
                Err("observed build control")
            } else {
                Ok(())
            }
        },
        &mut |_| Ok::<_, &'static str>(()),
    );
    assert!(matches!(failed, Err(NativeGraphBuildError::Control(_))));
    assert!(failed_work >= fire_at);
    let mut memory_events = 0_usize;
    let failed_memory = build_native_graph(
        2,
        &codes,
        &factors,
        &coordinates,
        GraphParams::sift_1m(),
        super::NATIVE_BUILD_SEED,
        &mut |_| Ok::<_, &'static str>(()),
        &mut |event| {
            memory_events += 1;
            if memory_events == 3 && matches!(event, BuildMemoryEvent::Acquire(_)) {
                Err("observed memory control")
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(
        failed_memory,
        Err(NativeGraphBuildError::Control(_))
    ));

    let directory = tempfile::tempdir().expect("temporary control store");
    let document = document_tower();
    let store = Store::create_native_graph(
        directory.path().join("native"),
        native_options(),
        Some(document.clone()),
    )
    .expect("fresh control store");
    let shared = GraphResources::from_store(&store).expect("control graph resources");
    let baseline = shared.reserved_bytes().expect("control baseline");
    let before = store.admit_native_read().expect("control before");
    let base = before.bundle().base();
    let sequence = before.bundle().sequence();
    drop(before);
    let cancel = CancelToken::new();
    cancel.cancel();
    let embedding = CanonicalEmbedding::new(&document, &[0.25, 0.75]).expect("embedding");
    let node = CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).expect("node");
    let request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "cancelled").expect("cancel key"),
        revision: GraphRevision::new(1).expect("cancel revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&node)),
    }];
    assert!(
        store
            .apply_native_graph(&request, &QueryControl::Cancel(cancel))
            .is_err()
    );
    let after = store.admit_native_read().expect("control after");
    assert_eq!(after.bundle().base(), base);
    assert_eq!(after.bundle().sequence(), sequence);
    drop(after);
    assert_eq!(
        shared.reserved_bytes().expect("released control bytes"),
        baseline
    );
    #[cfg(feature = "allocation-audit")]
    {
        let scope = super::test_support::install_build_allocation_failure(1);
        let allocation_failure =
            store.apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()));
        let fires = scope.fires();
        drop(scope);
        assert_eq!(
            fires, 1,
            "the first actual native graph backing allocation was denied"
        );
        let after = store.admit_native_read().expect("allocation failure state");
        assert_eq!(after.bundle().base(), base);
        assert_eq!(after.bundle().sequence(), sequence);
        drop(after);
        assert_eq!(
            shared.reserved_bytes().expect("allocation failure release"),
            baseline
        );
        let error = allocation_failure
            .err()
            .expect("actual allocator denial fails native apply");
        assert!(
            matches!(
                error,
                crate::lifecycle::native_graph::NativeGraphError::Read(TreeError::Memory)
            ),
            "actual native allocation error preserves memory cause: {error:?}"
        );
        let restored = store
            .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
            .expect("same-input native allocation clean control");
        assert_eq!(restored.len(), 1);
    }
    drop(clean);
    store.close().expect("close control store");

    let (limits, controls) = super::test_support::run_preparation_schedule_probe(0x158);
    assert_eq!(limits.key, "property-graph.native-vector-index.limit.fire");
    assert_eq!(
        controls.key,
        "property-graph.native-vector-index.control.fire"
    );
    assert_eq!(limits.work.kind, "work");
    assert_eq!(limits.memory.kind, "memory");
    assert_eq!(controls.cancel.kind, "cancelled");
    assert_eq!(controls.deadline.kind, "timeout");
    assert_eq!(controls.close.kind, "read-cancelled");
    assert!(limits.clean.completed && controls.clean.completed);
    assert!(controls.close.closing_observed);
    for observation in [
        &limits.work,
        &limits.memory,
        &controls.cancel,
        &controls.deadline,
    ] {
        assert!(observation.fired);
        assert_eq!(observation.generation_after, observation.generation_before);
        assert_eq!(observation.sequence_after, observation.sequence_before);
        assert_eq!(
            observation.released_reserved_bytes,
            observation.baseline_reserved_bytes
        );
        assert_eq!(
            observation.closed_reserved_bytes,
            limits.clean.closed_reserved_bytes
        );
    }
    assert_eq!(
        controls.close.closed_reserved_bytes,
        controls.clean.closed_reserved_bytes
    );
}

#[test]
fn native_vector_index_publication_reopen_required_refs() {
    use super::test_support::{NativePrepareStage, install};
    use crate::lifecycle::native_graph::tests::publication::{FaultPoint, RecordingVfs};
    use crate::vfs::Vfs;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn copy_store(source: &Path, target: &Path) {
        std::fs::create_dir_all(target).expect("create scratch store directory");
        for entry in std::fs::read_dir(source).expect("read clean store directory") {
            let entry = entry.expect("clean store entry");
            let destination = target.join(entry.file_name());
            if entry.file_type().expect("clean store entry type").is_dir() {
                copy_store(&entry.path(), &destination);
            } else {
                std::fs::copy(entry.path(), destination).expect("copy clean store entry");
            }
        }
    }

    fn remove_artifact(directory: &Path, reference: PhysicalRef) {
        std::fs::remove_file(crate::property_graph::storage::allocation::artifact_path(
            directory,
            reference.artifact,
        ))
        .expect("remove required native artifact");
    }

    fn assert_reopen_rejected(path: &Path, document: &EmbeddingTower) {
        if let Ok(store) = Store::open_native_graph(path, native_options(), Some(document.clone()))
        {
            store.close().expect("close unexpectedly admitted store");
            panic!("native store missing a required vector reference was admitted");
        }
    }

    fn corrupt_index(directory: &Path, reference: PhysicalRef) {
        let artifact = crate::property_graph::storage::allocation::artifact_path(
            directory,
            reference.artifact,
        );
        let metadata = std::fs::metadata(&artifact).expect("index artifact metadata");
        let mut permissions = metadata.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&artifact, permissions).expect("make corruption fixture writable");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&artifact)
            .expect("open index artifact");
        let payload_offset = reference.offset + 24;
        file.seek(SeekFrom::Start(payload_offset))
            .expect("seek index payload");
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).expect("read index byte");
        byte[0] ^= 0x80;
        file.seek(SeekFrom::Start(payload_offset))
            .expect("reseak index payload");
        file.write_all(&byte).expect("corrupt index byte");
        file.sync_all().expect("sync corrupt index artifact");
    }

    let fixture = super::test_support::prepare_reopen_fixture(0x158);
    let directory = &fixture;
    let document = fixture.document.clone();
    let coordinates = fixture.coordinates.clone();
    let query = fixture.query;
    let wal_path = fixture.wal_path.clone();
    let checkpoint_path = fixture.checkpoint_path.clone();
    let wal_before = fixture.wal_before.clone();
    let checkpoint_before = fixture.checkpoint_before.clone();

    let wal_missing_chunk = directory.path().join("wal-missing-index-chunk");
    let wal_missing_catalog = directory.path().join("wal-missing-index-catalog");
    let checkpoint_missing_chunk = directory.path().join("checkpoint-missing-index-chunk");
    let checkpoint_missing_catalog = directory.path().join("checkpoint-missing-index-catalog");
    let corrupt_path = directory.path().join("checkpoint-corrupt-index");
    copy_store(&wal_path, &wal_missing_chunk);
    copy_store(&wal_path, &wal_missing_catalog);
    copy_store(&checkpoint_path, &checkpoint_missing_chunk);
    copy_store(&checkpoint_path, &checkpoint_missing_catalog);
    copy_store(&checkpoint_path, &corrupt_path);

    let report = super::test_support::verify_reopen_fixture(&fixture);
    assert_eq!(report.key, "property-graph.native-vector-index.reopen");
    assert_eq!(report.wal_prepare_events, 0);
    assert_eq!(report.checkpoint_prepare_events, 0);
    assert_eq!(report.wal_before, report.wal_after);
    assert_eq!(report.checkpoint_before, report.checkpoint_after);

    remove_artifact(&wal_missing_chunk, wal_before.index_physical_references[1]);
    assert_reopen_rejected(&wal_missing_chunk, &document);
    remove_artifact(&wal_missing_catalog, wal_before.index_catalog_block);
    assert_reopen_rejected(&wal_missing_catalog, &document);
    remove_artifact(
        &checkpoint_missing_chunk,
        checkpoint_before.index_physical_references[1],
    );
    assert_reopen_rejected(&checkpoint_missing_chunk, &document);
    remove_artifact(
        &checkpoint_missing_catalog,
        checkpoint_before.index_catalog_block,
    );
    assert_reopen_rejected(&checkpoint_missing_catalog, &document);
    corrupt_index(&corrupt_path, checkpoint_before.index_reference);
    assert_reopen_rejected(&corrupt_path, &document);

    let fault_path = directory.path().join("object-sync-fault");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let fault_store = Store::create_native_graph_with_infrastructure(
        &fault_path,
        native_options(),
        Some(document.clone()),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh object-sync fault store");
    let fault_before = {
        let lease = fault_store
            .admit_native_read()
            .expect("object-sync state admission");
        let state = (lease.bundle().base(), lease.bundle().sequence());
        drop(lease);
        state
    };
    let prepared = Arc::new(AtomicBool::new(false));
    let receipt = Arc::clone(&prepared);
    let scope = install(None, None, move |event| {
        if event.stage == NativePrepareStage::Serialization {
            receipt.store(true, Ordering::SeqCst);
        }
    });
    vfs.arm_fault(FaultPoint::ObjectSync);
    assert!(
        try_apply_repeated_vectors(
            &fault_store,
            &document,
            "object-sync",
            24,
            &coordinates,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .is_err()
    );
    drop(scope);
    assert!(prepared.load(Ordering::SeqCst));
    vfs.assert_fired_once();
    let fault_after = {
        let lease = fault_store
            .admit_native_read()
            .expect("object-sync failure state admission");
        let state = (lease.bundle().base(), lease.bundle().sequence());
        drop(lease);
        state
    };
    assert_eq!(fault_after, fault_before);
    assert!(inspect_sources(&fault_store, query).is_empty());
    try_apply_repeated_vectors(
        &fault_store,
        &document,
        "object-sync",
        24,
        &coordinates,
        &QueryControl::Cancel(CancelToken::new()),
    )
    .expect("same-input object-sync clean control");
    let clean = inspect_sources(&fault_store, query);
    assert_eq!(clean.len(), 1);
    assert_eq!(clean[0].rows, 24);
    fault_store.close().expect("close object-sync fault store");
}

#[test]
fn native_vector_index_trace_complete_and_owner() {
    use crate::property_graph::staging::{WriteLimits, WriteMemory};
    use crate::property_graph::storage::memory::StorageMemory;
    use crate::property_graph::storage::tree::directory::BlockSource;
    use crate::property_graph::storage::{NativePreparationCatalog, NativePreparationSource};

    struct MissingReferenceSource<'a, S> {
        base: &'a S,
        missing: PhysicalRef,
    }
    impl<S: BlockSource> BlockSource for MissingReferenceSource<'_, S> {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            resources: &mut TreeResources<'_>,
        ) -> Result<crate::property_graph::storage::artifact::FramedBlock<'a>, TreeError> {
            if reference == self.missing {
                return Err(TreeError::Missing);
            }
            self.base.resolve(reference, resources)
        }
    }

    let fixture = super::test_support::prepare_trace_fixture(0x158);
    let trace_report = super::test_support::verify_trace_fixture(&fixture);
    assert_eq!(trace_report.key, "property-graph.native-vector-index.trace");
    assert!(trace_report.small.complete && trace_report.full.complete);
    assert_eq!(trace_report.small.references, trace_report.full.references);
    assert_eq!(
        trace_report.released_reserved_bytes,
        trace_report.baseline_reserved_bytes
    );
    let store = &fixture.store;
    let reports = &fixture.reports;
    let extent = reports
        .iter()
        .find(|report| report.rows == 24)
        .expect("extent-backed native vector index");

    let shared = GraphResources::from_store(store).expect("trace shared resources");
    let baseline = shared.reserved_bytes().expect("trace baseline");
    {
        let lease = store.admit_native_read().expect("trace admission");
        let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("trace writer");
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let memory =
            StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("trace storage memory");
        let source =
            NativePreparationSource::new(&lease, &memory, 128).expect("trace preparation source");
        let mut resources = source.resources(u64::MAX).expect("trace resources");
        let catalog = NativePreparationCatalog::open(&source, &mut resources)
            .expect("trace preparation catalog");

        let foreign_writer =
            WriteMemory::new(&shared, WriteLimits::default()).expect("foreign trace writer");
        let foreign_control = QueryControl::Cancel(CancelToken::new());
        let foreign_memory =
            StorageMemory::new(&foreign_writer, &foreign_control, 32 * 1024 * 1024)
                .expect("foreign trace memory");
        let foreign_source =
            NativePreparationSource::new(&lease, &foreign_memory, 1).expect("foreign trace source");
        let mut foreign_resources = foreign_source
            .resources(u64::MAX)
            .expect("foreign resources");
        {
            let mut owner_cursor =
                super::super::SearchTraceCursor::for_preparation(&source, &catalog, &mut resources)
                    .expect("owner trace cursor");
            assert!(matches!(
                owner_cursor.trace(&mut [None], &mut foreign_resources),
                Err(TreeError::Invalid(_))
            ));
        }
        drop(foreign_resources);
        drop(foreign_source);
        drop(foreign_memory);
        drop(foreign_control);

        let missing_reference = extent
            .index_payload
            .physical_reference_at(
                &source,
                lease.bundle().roots().store(),
                lease.bundle().base().generation,
                2,
                &mut resources,
            )
            .expect("late index descendant")
            .expect("extent has a second chunk");
        let missing_source = MissingReferenceSource {
            base: &source,
            missing: missing_reference,
        };
        let roots = super::super::SparseRoots {
            text: lease.bundle().text(),
            vector: lease.bundle().vector(),
        };
        let mut missing = super::super::SearchTraceCursor::for_test(
            &missing_source,
            &catalog,
            roots,
            lease.bundle().roots(),
            lease.bundle().catalog(),
            lease.bundle().document(),
            lease.bundle().lexical(),
            &lease,
            &memory,
            &mut resources,
        )
        .expect("missing-descendant trace cursor");
        let mut emitted = 0;
        loop {
            match missing.trace(&mut [None], &mut resources) {
                Ok(result) => {
                    assert!(!result.complete);
                    emitted += result.count;
                }
                Err(TreeError::Missing) => break,
                Err(other) => panic!("missing descendant trace returned {other}"),
            }
        }
        assert!(emitted > 0);
        drop(missing);

        let mut canceled =
            super::super::SearchTraceCursor::for_preparation(&source, &catalog, &mut resources)
                .expect("cancel trace cursor");
        let first = canceled
            .trace(&mut [None], &mut resources)
            .expect("first trace reference");
        assert_eq!(first.count, 1);
        assert!(!first.complete);
        token.cancel();
        assert!(canceled.trace(&mut [None], &mut resources).is_err());
        drop(canceled);
        drop(catalog);
        drop(resources);
        drop(source);
        drop(memory);
        drop(control);
        drop(lease);
    }
    assert_eq!(
        shared.reserved_bytes().expect("released trace resources"),
        baseline
    );
    store.close().expect("close trace store");
}

#[test]
fn native_vector_index_directed_probe_can_fire() {
    let report = super::test_support::run_oracle_probe(0x158);
    assert_eq!(
        report.key,
        "property-graph.native-vector-index.oracle.can-fire"
    );
    assert_eq!(report.fires, 6);
    assert_eq!(report.release_checks, 2);
    assert!(report.initial_clean && report.same_seed_clean);
    assert!(
        report
            .controls
            .iter()
            .all(|control| control.rejected && control.restored)
    );
    assert!(report.preparation_events > 0 && report.traversal_visits > 0);
    assert_eq!(report.expected_identities, report.observed_identities);
    assert_eq!(
        report.expected_coordinate_bits,
        report.observed_coordinate_bits
    );
    assert_eq!(report.observed_references, report.restored_references);
}
