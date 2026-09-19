#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization,
};

#[path = "../src/test_support.rs"]
mod test_support;

#[test]
fn seeded_property_permutation_is_an_equivalence_relation() {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use rand::RngCore;
    let mut seeded = test_support::seeded_rng("graph_canonical::permutation");
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(seeded.next_u64()),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(
            &(prop::collection::vec(any::<u64>(), 1..40), any::<usize>()),
            |(bits, shift)| {
                let names: Vec<_> = (0..bits.len())
                    .map(|index| format!("property-{index:02}"))
                    .collect();
                let values: Vec<_> = names
                    .iter()
                    .zip(&bits)
                    .map(|(name, bits)| property(name, PropertyData::F64(f64::from_bits(*bits))))
                    .collect();
                let mut first = values.clone();
                let mut second = values.clone();
                let length = second.len();
                second.rotate_left(shift % length);
                let mut third = values;
                third.reverse();
                let first = bytes(
                    &CanonicalContents::node(&mut [], &mut first, None, None).expect("first"),
                );
                let second = bytes(
                    &CanonicalContents::node(&mut [], &mut second, None, None).expect("second"),
                );
                let third = bytes(
                    &CanonicalContents::node(&mut [], &mut third, None, None).expect("third"),
                );
                prop_assert_eq!(&first, &second);
                prop_assert_eq!(&second, &third);
                prop_assert_eq!(&first, &third);
                let fp = CanonicalFingerprint::new(first.len() as u64, 0).expect("forced hash");
                prop_assert!(
                    compare(&first, &third, fp, &mut [0; 13])
                        .expect("exact")
                        .equal
                );
                let mut changed = first.clone();
                let index = shift % changed.len();
                changed[index] ^= 1;
                prop_assert!(
                    !compare(&first, &changed, fp, &mut [0; 13])
                        .expect("changed byte")
                        .equal
                );
                Ok(())
            },
        )
        .expect("256 seeded permutation/bit-change properties");
}
use zeppelin_embed::property_graph::{
    CanonicalComparison, CanonicalContents, CanonicalEmbedding, CanonicalError,
    CanonicalFingerprint, GraphName, GraphProperty, MAX_CANONICAL_SCRATCH, MAX_GRAPH_INPUT_BYTES,
    NodeId, PropertyData, PropertyValue, compare_canonical_streams,
};

fn property<'a>(name: &'a str, value: PropertyData<'a>) -> GraphProperty<'a> {
    GraphProperty::new(
        GraphName::new(name).expect("name"),
        PropertyValue::new(value).expect("value"),
    )
}

fn bytes(contents: &CanonicalContents<'_>) -> Vec<u8> {
    let mut output = Vec::new();
    let report = contents
        .write_to(&mut output, &mut || Ok(()))
        .expect("stream canonical bytes");
    assert_eq!(report.bytes, output.len() as u64);
    assert_eq!(contents.encoded_len(), output.len() as u64);
    output
}

fn value_bytes(value: PropertyData<'_>) -> Vec<u8> {
    bytes(&CanonicalContents::node(&mut [], &mut [property("", value)], None, None).expect("image"))
}

fn unhex(value: &str) -> Vec<u8> {
    value
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).expect("literal hex"))
        .collect()
}

#[test]
fn scalar_list_tags_and_float_payloads_have_literal_goldens() {
    // One node, no labels, one property with an empty name. The suffix is the
    // independent hand-written value layout followed by absent text/vector.
    let prefix = unhex(
        "5a 47 43 49 01 00 01 00 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
    );
    let floats = [
        f64::from_bits(0x8000_0000_0000_0000),
        f64::from_bits(0x7ff8_0000_0000_0042),
    ];
    let cases = [
        (
            PropertyData::String("é\0"),
            "01 03 00 00 00 00 00 00 00 c3 a9 00",
        ),
        (PropertyData::Bool(true), "02 01"),
        (PropertyData::Bool(false), "02 00"),
        (PropertyData::I64(i64::MIN), "03 00 00 00 00 00 00 00 80"),
        (PropertyData::F64(floats[1]), "04 42 00 00 00 00 00 f8 7f"),
        (
            PropertyData::EmptyList { count: 0 },
            "05 00 00 00 00 00 00 00 00",
        ),
        (
            PropertyData::Strings(&["", "é"]),
            "06 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00 c3 a9",
        ),
        (
            PropertyData::Bools(&[false, true]),
            "07 02 00 00 00 00 00 00 00 00 01",
        ),
        (
            PropertyData::Integers(&[i64::MIN, i64::MAX]),
            "08 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00 80 ff ff ff ff ff ff ff 7f",
        ),
        (
            PropertyData::Floats(&floats),
            "09 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00 80 42 00 00 00 00 00 f8 7f",
        ),
    ];
    for (data, literal) in cases {
        let expected = [prefix.as_slice(), unhex(literal).as_slice(), &[0, 0]].concat();
        assert_eq!(value_bytes(data), expected, "{data:?}");
    }
    let empties = [
        PropertyData::EmptyList { count: 0 },
        PropertyData::Strings(&[]),
        PropertyData::Bools(&[]),
        PropertyData::Integers(&[]),
        PropertyData::Floats(&[]),
    ];
    let encodings: std::collections::BTreeSet<_> = empties.into_iter().map(value_bytes).collect();
    assert_eq!(encodings.len(), 5, "typed empty lists cannot collapse");
    assert_ne!(
        value_bytes(PropertyData::I64(1)),
        value_bytes(PropertyData::F64(1.0))
    );
    assert_eq!(0.0_f64, -0.0_f64); // Numeric equality is intentionally not replay equality.
    assert_ne!(
        value_bytes(PropertyData::F64(0.0)),
        value_bytes(PropertyData::F64(-0.0))
    );
    let patterns = [
        0,
        1,
        0x8000_0000_0000_0000,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7ff0_0000_0000_0001,
        0x7ff8_0000_0000_0042,
        0x7ff8_0000_0000_0043,
    ];
    let unique: std::collections::BTreeSet<_> = patterns
        .into_iter()
        .map(|bits| value_bytes(PropertyData::F64(f64::from_bits(bits))))
        .collect();
    assert_eq!(unique.len(), patterns.len());
}

#[test]
fn full_width_directed_relationships_text_and_unicode_are_distinct() {
    let source = NodeId::new((1_u128 << 64) + 1).expect("source");
    let target = NodeId::new(1).expect("target");
    let relation = |s, t, kind| {
        bytes(
            &CanonicalContents::relationship(s, t, GraphName::new(kind).expect("type"), &mut [])
                .expect("relationship"),
        )
    };
    let baseline = relation(source, target, "é");
    let expected = unhex(
        "5a 47 43 49 01 00 02 01 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00 c3 a9 00 00 00 00 00 00 00 00 00 00",
    );
    assert_eq!(baseline, expected);
    assert_ne!(baseline, relation(target, source, "é"));
    assert_ne!(baseline, relation(source, target, "e\u{301}"));
    assert_ne!(baseline, relation(target, target, "é"));
    let node = |text| bytes(&CanonicalContents::node(&mut [], &mut [], text, None).expect("node"));
    let images: std::collections::BTreeSet<_> =
        [None, Some(""), Some("é"), Some("e\u{301}"), Some("\0")]
            .into_iter()
            .map(node)
            .chain([baseline])
            .collect();
    assert_eq!(images.len(), 6);
    let prop = property("é", PropertyData::Bool(true));
    assert_eq!(prop.name().as_str(), "é");
    assert!(matches!(prop.value().data(), PropertyData::Bool(true)));
}

fn tower() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "doc".into(),
        model_version: "1".into(),
        weights_digest: vec![0x42],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "".into(),
        max_tokens: 3,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}

fn vector_bytes(document: &EmbeddingTower, vector: &[f32]) -> Vec<u8> {
    let embedding = CanonicalEmbedding::new(document, vector).expect("embedding");
    assert_eq!(embedding.document(), document);
    assert_eq!(embedding.vector().coordinates(), vector);
    bytes(&CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).expect("vector image"))
}

#[test]
fn original_vector_bits_and_every_document_field_determine_contents() {
    let document = tower();
    let baseline = vector_bytes(&document, &[1.0, -0.0]);
    assert_eq!(
        baseline,
        unhex(
            "5a 47 43 49 01 00 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 01 03 00 00 00 00 00 00 00 64 6f 63 01 00 00 00 00 00 00 00 31 01 00 00 00 00 00 00 00 42 02 00 00 00 00 00 00 00 00 00 00 00 00 00 03 00 00 00 03 00 01 00 00 02 00 00 00 00 00 00 00 00 00 80 3f 00 00 00 80"
        )
    );
    assert_ne!(baseline, vector_bytes(&document, &[1.0, 0.0]));
    assert_ne!(
        baseline,
        vector_bytes(&document, &[f32::from_bits(1.0_f32.to_bits() + 1), -0.0])
    );
    let mut first_code = [0];
    let mut second_code = [0];
    zeppelin_embed::quant::quantize_bit4(&[1.0, 0.0001], &mut first_code)
        .expect("first quantization");
    zeppelin_embed::quant::quantize_bit4(&[1.0, 0.00010001], &mut second_code)
        .expect("second quantization");
    assert_eq!(
        first_code, second_code,
        "different supplied vectors share stored search codes"
    );
    assert_ne!(
        vector_bytes(&document, &[1.0, 0.0001]),
        vector_bytes(&document, &[1.0, 0.00010001]),
        "quantized codes cannot prove replay equality"
    );
    for index in 0..10 {
        let mut changed = document.clone();
        match index {
            0 => changed.model_id.push('x'),
            1 => changed.model_version.push('x'),
            2 => changed.weights_digest.push(0),
            3 => changed.dims = 1,
            4 => changed.normalization = Normalization::L2,
            5 => changed.prompt_prefix.push('x'),
            6 => changed.max_tokens += 1,
            7 => changed.runtime = EmbeddingRuntime::Mlx,
            8 => changed.compute_units = ComputeUnits::All,
            _ => changed.os_build = Some(String::new()),
        }
        let vector: &[f32] = if index == 3 { &[1.0] } else { &[1.0, -0.0] };
        assert_ne!(
            baseline,
            vector_bytes(&changed, vector),
            "document field {index}"
        );
    }
    let mut pair = EmbeddingEpoch {
        document: document.clone(),
        query: document,
        alignment_digest: vec![1],
    };
    pair.query.runtime = EmbeddingRuntime::CoreMl;
    pair.alignment_digest = vec![2];
    assert_eq!(
        baseline,
        vector_bytes(&pair.document, &[1.0, -0.0]),
        "a query-only swap leaves stored document interpretation intact"
    );
    for vector in [&[f32::NAN, 0.0][..], &[f32::INFINITY, 0.0][..], &[1.0][..]] {
        assert!(matches!(
            CanonicalEmbedding::new(&pair.document, vector),
            Err(CanonicalError::Domain(_))
        ));
    }
}

fn compare(
    left: &[u8],
    right: &[u8],
    fingerprint: CanonicalFingerprint,
    scratch: &mut [u8],
) -> Result<CanonicalComparison, CanonicalError> {
    compare_canonical_streams(
        &mut Cursor::new(left),
        fingerprint,
        &mut Cursor::new(right),
        fingerprint,
        scratch,
        &mut || Ok(()),
    )
}

#[test]
fn forced_hash_collision_requires_every_byte_and_relocated_sources_compare_equal() {
    let original = value_bytes(PropertyData::F64(-0.0));
    let changed = value_bytes(PropertyData::F64(0.0));
    let forced = CanonicalFingerprint::new(original.len() as u64, 42).expect("forced equal hash");
    assert_eq!(forced.bytes(), original.len() as u64);
    assert_eq!(forced.hash(), 42);
    for size in [2, 3, 17, MAX_CANONICAL_SCRATCH] {
        let mut scratch = vec![0; size];
        assert!(
            !compare(&original, &changed, forced, &mut scratch)
                .expect("collision comparison")
                .equal
        );
        let equal = compare(&original, &original, forced, &mut scratch).expect("equal");
        assert!(equal.equal);
        assert_eq!(equal.bytes_compared, original.len() as u64);
    }
    let mut file = tempfile::tempfile().expect("physical source");
    file.write_all(&[0xaa; 37]).expect("first offset");
    file.write_all(&original).expect("first image");
    file.write_all(&[0xbb; 1001]).expect("relocation gap");
    let relocated = file.stream_position().expect("second location");
    file.write_all(&original).expect("relocated image");
    let mut other = file.try_clone().expect("source clone");
    // Cloned files share an OS offset, so separate reads are captured with seek
    // on each access, exactly as a borrowed packed-blob reader would do.
    struct At<'a> {
        file: &'a mut std::fs::File,
        offset: u64,
        remaining: u64,
    }
    impl Read for At<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.file.seek(SeekFrom::Start(self.offset))?;
            let count = self.file.take(self.remaining).read(output)?;
            self.offset += count as u64;
            self.remaining -= count as u64;
            Ok(count)
        }
    }
    let mut left = At {
        file: &mut file,
        offset: 37,
        remaining: forced.bytes(),
    };
    let mut right = At {
        file: &mut other,
        offset: relocated,
        remaining: forced.bytes(),
    };
    assert!(
        compare_canonical_streams(
            &mut left,
            forced,
            &mut right,
            forced,
            &mut [0; 16],
            &mut || Ok(())
        )
        .expect("relocated contents")
        .equal
    );
}

#[test]
fn stream_failures_cancellation_and_boundaries_fail_loudly() {
    let original = value_bytes(PropertyData::String("hello"));
    let fp = CanonicalFingerprint::new(original.len() as u64, 7).expect("metadata");
    for length in 0..original.len() {
        assert!(
            matches!(compare(&original[..length], &original, fp, &mut [0; 7]), Err(CanonicalError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
        );
    }
    let trailing = [original.as_slice(), &[0]].concat();
    for (left, right) in [
        (trailing.as_slice(), original.as_slice()),
        (original.as_slice(), trailing.as_slice()),
    ] {
        assert!(
            matches!(compare(left, right, fp, &mut [0; 7]), Err(CanonicalError::Io(error)) if error.kind() == io::ErrorKind::InvalidData)
        );
    }
    for size in [0, 1, MAX_CANONICAL_SCRATCH + 1] {
        assert!(matches!(
            compare(&original, &original, fp, &mut vec![0; size]),
            Err(CanonicalError::InvalidScratch)
        ));
    }
    assert!(matches!(
        CanonicalFingerprint::new(MAX_GRAPH_INPUT_BYTES as u64 + 1, 0),
        Err(CanonicalError::InputTooLarge)
    ));
    struct Failed;
    impl Read for Failed {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected read"))
        }
    }
    impl Write for Failed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected write"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(matches!(
        compare_canonical_streams(
            &mut Failed,
            fp,
            &mut Failed,
            fp,
            &mut [0; 2],
            &mut || Ok(())
        ),
        Err(CanonicalError::Io(_))
    ));
    for mismatch in [
        CanonicalFingerprint::new(fp.bytes(), 8).expect("hash"),
        CanonicalFingerprint::new(fp.bytes() + 1, 7).expect("length"),
    ] {
        assert_eq!(
            compare_canonical_streams(
                &mut Failed,
                fp,
                &mut Failed,
                mismatch,
                &mut [0; 2],
                &mut || Ok(())
            )
            .expect("mismatch before I/O"),
            CanonicalComparison {
                equal: false,
                bytes_compared: 0
            }
        );
    }
    let mut labels = [];
    let mut props = [property("a", PropertyData::String("hello"))];
    let image = CanonicalContents::node(&mut labels, &mut props, None, None).expect("image");
    assert!(matches!(
        image.write_to(&mut Failed, &mut || Ok(())),
        Err(CanonicalError::Io(_))
    ));
    let count = image
        .write_to(&mut io::sink(), &mut || Ok(()))
        .expect("control")
        .checkpoints;
    for fail_at in 0..count {
        let mut visits = 0;
        let mut checkpoint = || {
            let current = visits;
            visits += 1;
            if current == fail_at {
                Err(CanonicalError::Cancelled)
            } else {
                Ok(())
            }
        };
        assert!(matches!(
            image.write_to(&mut io::sink(), &mut checkpoint),
            Err(CanonicalError::Cancelled)
        ));
        assert_eq!(visits, fail_at + 1);
    }
    let mut visits = 0;
    compare_canonical_streams(
        &mut original.as_slice(),
        fp,
        &mut original.as_slice(),
        fp,
        &mut [0; 7],
        &mut || {
            visits += 1;
            Ok(())
        },
    )
    .expect("comparison control");
    for fail_at in 0..visits {
        let mut seen = 0;
        assert!(matches!(
            compare_canonical_streams(
                &mut original.as_slice(),
                fp,
                &mut original.as_slice(),
                fp,
                &mut [0; 7],
                &mut || {
                    let current = seen;
                    seen += 1;
                    if current == fail_at {
                        Err(CanonicalError::Cancelled)
                    } else {
                        Ok(())
                    }
                }
            ),
            Err(CanonicalError::Cancelled)
        ));
        assert_eq!(seen, fail_at + 1);
    }
    assert!(matches!(
        image.fingerprint(&mut || Err(CanonicalError::Cancelled)),
        Err(CanonicalError::Cancelled)
    ));
    let actual = image.fingerprint(&mut || Ok(())).expect("fingerprint");
    assert_eq!(actual.bytes(), image.encoded_len());
    assert_eq!(actual.hash(), xxhash_rust::xxh3::xxh3_64(&bytes(&image)));
}

#[test]
fn framing_is_charged_to_the_eight_mib_limit() {
    // Empty node framing=25 bytes; a present text adds its eight-byte length.
    let text = "x".repeat(MAX_GRAPH_INPUT_BYTES - 33);
    let mut labels = [];
    let mut props = [];
    let image =
        CanonicalContents::node(&mut labels, &mut props, Some(&text), None).expect("exact budget");
    assert_eq!(image.encoded_len(), MAX_GRAPH_INPUT_BYTES as u64);
    struct Bounded {
        bytes: usize,
        largest: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            self.bytes += input.len();
            self.largest = self.largest.max(input.len());
            Ok(input.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Bounded {
        bytes: 0,
        largest: 0,
    };
    image
        .write_to(&mut sink, &mut || Ok(()))
        .expect("bounded stream");
    assert_eq!(sink.bytes, MAX_GRAPH_INPUT_BYTES);
    assert_eq!(sink.largest, MAX_CANONICAL_SCRATCH);
    let too_large = text + "x";
    assert!(matches!(
        CanonicalContents::node(&mut [], &mut [], Some(&too_large), None),
        Err(CanonicalError::InputTooLarge)
    ));
    let maximum_name = "x".repeat(MAX_GRAPH_INPUT_BYTES);
    assert!(matches!(
        CanonicalContents::node(
            &mut [GraphName::new(&maximum_name).expect("raw name")],
            &mut [],
            None,
            None
        ),
        Err(CanonicalError::InputTooLarge)
    ));
    assert!(matches!(
        CanonicalContents::node(
            &mut [],
            &mut [property("", PropertyData::String(&maximum_name))],
            None,
            None
        ),
        Err(CanonicalError::InputTooLarge)
    ));
}

#[test]
fn canonical_property_order_and_labels_are_byte_exact() {
    let mut labels = [
        GraphName::new("é").expect("label"),
        GraphName::new("A").expect("label"),
        GraphName::new("A").expect("duplicate label"),
    ];
    let mut properties = [
        property("z", PropertyData::I64(7)),
        property("a\0", PropertyData::String("é")),
    ];
    let first =
        CanonicalContents::node(&mut labels, &mut properties, None, None).expect("normalize first");
    let mut other_labels = [
        GraphName::new("A").expect("label"),
        GraphName::new("é").expect("label"),
    ];
    let mut other_properties = [
        property("a\0", PropertyData::String("é")),
        property("z", PropertyData::I64(7)),
    ];
    let second = CanonicalContents::node(&mut other_labels, &mut other_properties, None, None)
        .expect("normalize reordered");
    assert_eq!(bytes(&first), bytes(&second));
    let mut repeated = [
        property("a", PropertyData::I64(1)),
        property("a", PropertyData::I64(1)),
    ];
    assert!(matches!(
        CanonicalContents::node(&mut [], &mut repeated, None, None),
        Err(CanonicalError::DuplicateProperty)
    ));
    let mut conflicting = [
        property("a", PropertyData::I64(1)),
        property("a", PropertyData::I64(2)),
    ];
    assert!(matches!(
        CanonicalContents::node(&mut [], &mut conflicting, None, None),
        Err(CanonicalError::DuplicateProperty)
    ));
}

#[test]
fn replay_provenance_fields_are_versioned_and_never_defaulted() {
    use zeppelin_embed::property_graph::{
        ApplicationKey, EntityId, EntityKind, ExpectedGraphState, GraphGeneration, GraphOperation,
        GraphRevision, NodeId, OperationFields, OperationProvenance,
    };
    let id = EntityId::Node(NodeId::new((1_u128 << 64) + 1).expect("full ID"));
    let fields = OperationFields {
        operation: GraphOperation::StructuredPut,
        key: Some(ApplicationKey::new(EntityKind::Node, "n", "k").expect("key")),
        requested_revision: GraphRevision::new(7).expect("revision"),
        installed_revision: GraphRevision::new(7).expect("revision"),
        expected: ExpectedGraphState::Entity(id),
        incarnation: id,
        delete_mode: None,
        original_generation: GraphGeneration::new(42),
    };
    assert!(matches!(
        OperationProvenance::from_fields(None, fields),
        Err(CanonicalError::UnsupportedProvenanceVersion)
    ));
    assert!(matches!(
        OperationProvenance::from_fields(Some(2), fields),
        Err(CanonicalError::UnsupportedProvenanceVersion)
    ));
    let provenance =
        OperationProvenance::from_fields(Some(1), fields).expect("supported complete record");
    let mut encoded = Vec::new();
    provenance
        .write_to(&mut encoded, &mut || Ok(()))
        .expect("canonical provenance");
    assert_eq!(&encoded[..8], &[b'Z', b'G', b'O', b'P', 1, 0, 2, 1]);
    assert_eq!(
        encoded,
        unhex(
            "5a 47 4f 50 01 00 02 01 01 01 00 00 00 00 00 00 00 6e 01 00 00 00 00 00 00 00 6b 07 00 00 00 00 00 00 00 07 00 00 00 00 00 00 00 02 01 01 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 01 01 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 00 2a 00 00 00 00 00 00 00"
        )
    );
    assert_eq!(provenance.version(), 1);
    assert_eq!(provenance.fields(), fields);
    assert_eq!(encoded.len() as u64, provenance.encoded_len());
}

#[test]
fn same_revision_replay_requires_contents_and_all_operation_fields() {
    use zeppelin_embed::property_graph::{
        ApplicationKey, EntityId, EntityKind, ExpectedGraphState, GraphDeleteMode, GraphGeneration,
        GraphOperation, GraphRevision, OperationFields, OperationProvenance, RelId, ReplayEvidence,
        compare_replay_evidence,
    };
    let id = EntityId::Node(NodeId::new((1_u128 << 64) + 1).expect("ID"));
    let base = OperationFields {
        operation: GraphOperation::StructuredPut,
        key: Some(ApplicationKey::new(EntityKind::Node, "ns", "key").expect("key")),
        requested_revision: GraphRevision::new(7).expect("revision"),
        installed_revision: GraphRevision::new(7).expect("revision"),
        expected: ExpectedGraphState::Entity(id),
        incarnation: id,
        delete_mode: None,
        original_generation: GraphGeneration::new(42),
    };
    let provenance = |fields| OperationProvenance::from_fields(Some(1), fields).expect("record");
    let original = value_bytes(PropertyData::F64(-0.0));
    let changed = value_bytes(PropertyData::F64(0.0));
    let forced = CanonicalFingerprint::new(original.len() as u64, 0).expect("forced collision");
    let compare = |fields, data: &[u8]| {
        compare_replay_evidence(
            ReplayEvidence {
                provenance: provenance(base),
                fingerprint: forced,
                contents: &mut original.as_slice(),
            },
            ReplayEvidence {
                provenance: provenance(fields),
                fingerprint: forced,
                contents: &mut &*data,
            },
            &mut [0; 19],
            &mut || Ok(()),
        )
        .expect("compare")
    };
    assert!(compare(base, &original).equal);
    assert!(
        !compare(base, &changed).equal,
        "same revision and hash, changed content"
    );
    for index in 0..15 {
        let mut changed = base;
        match index {
            0 => changed.operation = GraphOperation::StructuredCreate,
            1 => changed.operation = GraphOperation::StructuredDelete,
            2 => changed.operation = GraphOperation::StructuredRecreate,
            3 => changed.operation = GraphOperation::CypherEdit,
            4 => changed.key = None,
            5 => {
                changed.key =
                    Some(ApplicationKey::new(EntityKind::Node, "nsx", "key").expect("namespace"))
            }
            6 => {
                changed.key =
                    Some(ApplicationKey::new(EntityKind::Node, "ns", "keyx").expect("key"))
            }
            7 => changed.requested_revision = GraphRevision::new(8).expect("revision"),
            8 => changed.installed_revision = GraphRevision::new(8).expect("revision"),
            9 => changed.expected = ExpectedGraphState::Absent,
            10 => {
                changed.expected =
                    ExpectedGraphState::Deletion(GraphRevision::new(6).expect("deletion"))
            }
            11 => changed.incarnation = EntityId::Node(NodeId::new(1).expect("different full ID")),
            12 => changed.delete_mode = Some(GraphDeleteMode::Restrict),
            13 => changed.delete_mode = Some(GraphDeleteMode::Detach),
            _ => changed.original_generation = GraphGeneration::new(43),
        }
        assert_eq!(
            compare(changed, &original),
            CanonicalComparison {
                equal: false,
                bytes_compared: 0
            },
            "field {index}"
        );
        let mut first = Vec::new();
        let mut second = Vec::new();
        provenance(base)
            .write_to(&mut first, &mut || Ok(()))
            .expect("base stream");
        provenance(changed)
            .write_to(&mut second, &mut || Ok(()))
            .expect("changed stream");
        assert_ne!(
            first, second,
            "field must also survive serialization {index}"
        );
    }
    let relation = EntityId::Relationship(RelId::new((1_u128 << 64) + 1).expect("relationship"));
    let mut other = base;
    other.incarnation = relation;
    assert!(matches!(
        OperationProvenance::from_fields(Some(1), other),
        Err(CanonicalError::ProvenanceKindMismatch)
    ));
    other.key =
        Some(ApplicationKey::new(EntityKind::Relationship, "ns", "key").expect("relationship key"));
    assert!(matches!(
        OperationProvenance::from_fields(Some(1), other),
        Err(CanonicalError::ProvenanceKindMismatch)
    ));
    other.expected = ExpectedGraphState::Entity(relation);
    assert!(
        !compare(other, &original).equal,
        "kind scopes otherwise identical full keys and bits"
    );
    assert!(matches!(
        compare_replay_evidence(
            ReplayEvidence {
                provenance: provenance(base),
                fingerprint: forced,
                contents: &mut original.as_slice()
            },
            ReplayEvidence {
                provenance: provenance(base),
                fingerprint: forced,
                contents: &mut original.as_slice()
            },
            &mut [0; 2],
            &mut || Err(CanonicalError::Cancelled)
        ),
        Err(CanonicalError::Cancelled)
    ));
    let count = provenance(base)
        .write_to(&mut io::sink(), &mut || Ok(()))
        .expect("control")
        .checkpoints;
    for fail_at in 0..count {
        let mut visited = 0;
        assert!(matches!(
            provenance(base).write_to(&mut io::sink(), &mut || {
                let i = visited;
                visited += 1;
                if i == fail_at {
                    Err(CanonicalError::Cancelled)
                } else {
                    Ok(())
                }
            }),
            Err(CanonicalError::Cancelled)
        ));
        assert_eq!(visited, fail_at + 1);
    }
    let maximum_key = "x".repeat(MAX_GRAPH_INPUT_BYTES);
    let mut huge = base;
    huge.key =
        Some(ApplicationKey::new(EntityKind::Node, "", &maximum_key).expect("raw maximum key"));
    assert!(matches!(
        OperationProvenance::from_fields(Some(1), huge),
        Err(CanonicalError::InputTooLarge)
    ));
}

#[test]
fn short_io_and_interrupted_reads_preserve_exact_streaming() {
    struct ShortRead<'a> {
        source: &'a [u8],
        interrupt: bool,
    }
    impl Read for ShortRead<'_> {
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
            if self.interrupt {
                self.interrupt = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let length = target.len().min(3);
            self.source.read(&mut target[..length])
        }
    }
    struct ShortWrite(Vec<u8>);
    impl Write for ShortWrite {
        fn write(&mut self, source: &[u8]) -> io::Result<usize> {
            let count = source.len().min(3);
            self.0.extend_from_slice(&source[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut labels = [];
    let mut props = [property("x", PropertyData::I64(42))];
    let image = CanonicalContents::node(&mut labels, &mut props, None, None).expect("image");
    let encoded = bytes(&image);
    let mut sink = ShortWrite(Vec::new());
    image
        .write_to(&mut sink, &mut || Ok(()))
        .expect("short writes");
    assert_eq!(sink.0, encoded);
    let mut left = ShortRead {
        source: &encoded,
        interrupt: true,
    };
    let mut right = ShortRead {
        source: &encoded,
        interrupt: true,
    };
    let fp = image.fingerprint(&mut || Ok(())).expect("hash");
    assert!(
        compare_canonical_streams(&mut left, fp, &mut right, fp, &mut [0; 31], &mut || Ok(()))
            .expect("short interrupted reads")
            .equal
    );
}
