//! Task 13 frozen bytes: postings, dictionary, and norms.
//!
//! These fixtures are the persisted meaning of the lexical index. A change
//! here is a format change, not a test failure to be re-blessed. In
//! particular the postings-per-block value is a HEADER FIELD: retuning it
//! (task 27-B4 sweeps 32/40/64/128) writes a different header and a
//! different golden, and that is the intended, visible cost.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::collections::BTreeMap;

use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::fts::dict::{TERMS_PER_BLOCK, TermDictionary, TermInfo};
use zeppelin_embed::fts::norms::{Norms, encode_length};
use zeppelin_embed::fts::postings::{
    BLOCK_META_LEN, BlockImpact, DEFAULT_POSTINGS_PER_BLOCK, POSTINGS_VERSION, POSTINGS_VERSION_V1,
    Posting, PostingList, PostingsReader, block_impacts, encode, encode_v2,
};

fn fixture(text: &str) -> Vec<u8> {
    decode_hex(text).expect("fixture hex")
}

fn render_hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && index % 16 == 0 {
            out.push('\n');
        } else if index > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out.push('\n');
    out
}

/// The frozen posting list. Small, but it exercises every stream.
fn golden_list() -> PostingList {
    let mut list = PostingList::new();
    for (docid, positions) in [
        (0_u32, &[0_u32, 4][..]),
        (1, &[7][..]),
        (5, &[2, 3, 11][..]),
        (300, &[0][..]),
        (301, &[1, 2][..]),
    ] {
        list.push(Posting {
            docid,
            tf: u32::try_from(positions.len()).expect("small"),
            positions: positions.to_vec(),
        })
        .expect("ascending fixture");
    }
    list
}

fn golden_dictionary() -> TermDictionary {
    let mut entries = BTreeMap::new();
    for (index, term) in [
        "engine",
        "engineer",
        "engineering",
        "engines",
        "lexical",
        "search",
    ]
    .into_iter()
    .enumerate()
    {
        entries.insert(
            term.as_bytes().to_vec(),
            TermInfo {
                document_frequency: u32::try_from(index + 1).expect("small"),
                postings_offset: u64::try_from(index * 32).expect("small"),
                postings_length: 32,
            },
        );
    }
    TermDictionary::from_map(&entries)
}

fn golden_norms() -> Norms {
    Norms::from_lengths(&[0, 1, 39, 40, 41, 1_000, 65_000, u32::MAX])
}

#[test]
fn sealed_postings_golden_bytes_are_frozen() {
    // Block size 2 so the fixture spans three blocks and freezes the
    // multi-block metadata layout, not just a single-block special case.
    let encoded = encode(&golden_list(), 2, &[17, 200, 255]).expect("encodes");
    assert_eq!(
        render_hex(encoded.as_bytes()),
        include_str!("fixtures/format/fts_postings_v1.hex"),
        "the sealed posting layout changed"
    );

    let reader = PostingsReader::open(encoded.as_bytes()).expect("golden postings decode");
    assert_eq!(reader.postings_per_block(), 2);
    assert_eq!(reader.document_frequency(), 5);
    assert_eq!(reader.blocks().len(), 3);
    assert_eq!(reader.decode_all().expect("decodes"), golden_list());

    // The block maxima ride along untouched, for task 14 to read.
    let maxima: Vec<u8> = reader
        .blocks()
        .iter()
        .map(|block| block.block_max)
        .collect();
    assert_eq!(maxima, vec![17, 200, 255]);
}

/// The dense row lengths the v2 fixture's impact pairs are computed from.
fn golden_lengths() -> Vec<u32> {
    let mut lengths = vec![0_u32; 302];
    for (row, length) in [(0, 40_u32), (1, 12), (5, 900), (300, 7), (301, 65_600)] {
        lengths[row] = length;
    }
    lengths
}

#[test]
fn sealed_postings_v2_golden_bytes_are_frozen() {
    // The same fixture, the same geometry, the same reserved u8 slot: the
    // ONLY difference from the v1 golden is the version word and the six
    // formerly-zero bytes per block that now carry the impact pair. Freezing
    // both is what proves version 2 is additive rather than a rewrite.
    let list = golden_list();
    let lengths = golden_lengths();
    let impacts = block_impacts(&list, 2, &lengths);
    assert_eq!(
        impacts,
        vec![
            BlockImpact {
                max_tf: 2,
                min_len: 12
            },
            BlockImpact {
                max_tf: 3,
                min_len: 7
            },
            // Row 301 is 65,600 tokens long, past the u16 slot, so the
            // stored length saturates DOWN and the bound stays above the
            // truth rather than below it.
            BlockImpact {
                max_tf: 2,
                min_len: u16::MAX
            },
        ]
    );

    let encoded = encode_v2(&list, 2, &[17, 200, 255], &impacts).expect("encodes");
    assert_eq!(
        render_hex(encoded.as_bytes()),
        include_str!("fixtures/format/fts_postings_v2.hex"),
        "the version 2 posting layout changed"
    );

    let reader = PostingsReader::open(encoded.as_bytes()).expect("golden v2 decode");
    assert_eq!(reader.version(), POSTINGS_VERSION);
    assert_eq!(reader.decode_all().expect("decodes"), list);
    let read: Vec<BlockImpact> = reader.blocks().iter().map(BlockImpact::from_meta).collect();
    assert_eq!(read, impacts);

    // Byte for byte, v1 and v2 differ only in the version word and the six
    // reserved bytes of each metadata row.
    let v1 = encode(&list, 2, &[17, 200, 255]).expect("encodes");
    let old = v1.as_bytes();
    let new = encoded.as_bytes();
    assert_eq!(old.len(), new.len(), "version 2 must not resize a stream");
    for (offset, (left, right)) in old.iter().zip(new.iter()).enumerate() {
        if left == right {
            continue;
        }
        let differs = (4..6).contains(&offset)
            || (16..16 + 3 * BLOCK_META_LEN).contains(&offset)
                && (26..32).contains(&((offset - 16) % BLOCK_META_LEN));
        assert!(
            differs,
            "version 2 moved byte {offset}, which is neither the version \
             word nor a reserved metadata byte"
        );
    }
    assert_eq!(
        PostingsReader::open(old).expect("v1 opens").version(),
        POSTINGS_VERSION_V1
    );
}

#[test]
fn sealed_dictionary_golden_bytes_are_frozen() {
    let dictionary = golden_dictionary();
    assert_eq!(
        render_hex(dictionary.encoded_bytes()),
        include_str!("fixtures/format/fts_dictionary_v1.hex"),
        "the front-coded dictionary layout changed"
    );
    let decoded = dictionary
        .decode_terms()
        .expect("golden dictionary decodes");
    let expected: Vec<Vec<u8>> = [
        "engine",
        "engineer",
        "engineering",
        "engines",
        "lexical",
        "search",
    ]
    .into_iter()
    .map(|term| term.as_bytes().to_vec())
    .collect();
    assert_eq!(decoded, expected);
}

#[test]
fn sealed_norms_golden_bytes_are_frozen() {
    let norms = golden_norms();
    assert_eq!(
        render_hex(norms.as_bytes()),
        include_str!("fixtures/format/fts_norms_v1.hex"),
        "the norm quantization table changed"
    );
    // Decoding must never understate a length, or an upper bound built from
    // it would be unsound.
    for (row, length) in [0_u32, 1, 39, 40, 41, 1_000, 65_000, u32::MAX]
        .into_iter()
        .enumerate()
    {
        let restored = norms
            .length(u32::try_from(row).expect("small"))
            .expect("row present");
        assert!(
            restored >= length,
            "row {row} decoded {restored} below its true length {length}"
        );
    }
}

#[test]
fn the_persisted_geometry_constants_are_the_ones_the_goldens_were_built_with() {
    // If someone retunes the block size, this fails first and points at the
    // header field rather than letting a golden quietly drift.
    assert_eq!(DEFAULT_POSTINGS_PER_BLOCK, 64);
    assert_eq!(BLOCK_META_LEN, 32);
    assert_eq!(TERMS_PER_BLOCK, 16);
    assert_eq!(encode_length(0), 0);
    assert_eq!(encode_length(39), 39);
}

#[test]
fn the_default_geometry_also_round_trips_through_its_own_golden() {
    let encoded = encode(&golden_list(), DEFAULT_POSTINGS_PER_BLOCK, &[])
        .expect("encodes at the shipped geometry");
    assert_eq!(
        render_hex(encoded.as_bytes()),
        include_str!("fixtures/format/fts_postings_default_geometry_v1.hex"),
        "the shipped-geometry posting layout changed"
    );
    let reader = PostingsReader::open(encoded.as_bytes()).expect("decodes");
    assert_eq!(reader.postings_per_block(), DEFAULT_POSTINGS_PER_BLOCK);
    assert_eq!(reader.blocks().len(), 1);
    assert_eq!(reader.decode_all().expect("decodes"), golden_list());
}

/// Writes the golden fixtures. Not a gate; a generator.
///
/// ```text
/// cargo test -p zeppelin-embed --test fts_format_golden \
///     regenerate_fts_goldens -- --ignored
/// ```
#[test]
#[ignore = "generator, not a gate; run deliberately on a format change"]
fn regenerate_fts_goldens() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format");
    let postings = encode(&golden_list(), 2, &[17, 200, 255]).expect("encodes");
    std::fs::write(
        dir.join("fts_postings_v1.hex"),
        render_hex(postings.as_bytes()),
    )
    .expect("write postings golden");
    let shipped = encode(&golden_list(), DEFAULT_POSTINGS_PER_BLOCK, &[]).expect("encodes");
    std::fs::write(
        dir.join("fts_postings_default_geometry_v1.hex"),
        render_hex(shipped.as_bytes()),
    )
    .expect("write shipped-geometry golden");
    std::fs::write(
        dir.join("fts_dictionary_v1.hex"),
        render_hex(golden_dictionary().encoded_bytes()),
    )
    .expect("write dictionary golden");
    std::fs::write(
        dir.join("fts_norms_v1.hex"),
        render_hex(golden_norms().as_bytes()),
    )
    .expect("write norms golden");
}

#[test]
fn the_task_07_postings_reservation_stays_byte_identical() {
    // Task 07 reserved region kind 6 and committed this fixture before any
    // payload existed, so it is deliberately EMPTY: the reservation spends
    // the id without spending file space. Task 13 defines the payload in the
    // separate goldens above and must not disturb the reservation.
    assert_eq!(
        fixture(include_str!("fixtures/format/postings_reserved_v1.hex")),
        Vec::<u8>::new(),
        "the task 07 postings reservation is no longer empty"
    );
}
