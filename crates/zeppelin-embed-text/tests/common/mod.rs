#![allow(dead_code)]

use std::path::Path;

use zeppelin_embed::epoch::{EmbeddingEpoch, EpochIdentity, StoreEpoch};
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed_text::bundle::Bundle;

const HEADER_LEN: usize = 4096;

#[derive(Clone)]
pub struct FixtureTower<'a> {
    pub role: u8,
    pub model_id: &'a str,
    pub prefix: &'a str,
    pub architecture: u16,
    pub pooling: u8,
    pub dims: u32,
    pub word_vectors: [[f32; 2]; 7],
}

impl FixtureTower<'_> {
    pub fn document(model_id: &str) -> FixtureTower<'_> {
        FixtureTower {
            role: 0,
            model_id,
            prefix: "",
            architecture: 1,
            pooling: 1,
            dims: 2,
            word_vectors: [
                [0.0, 0.0],
                [0.0, 0.0],
                [1.0, 0.0],
                [0.0, 0.0],
                [0.0, 0.0],
                [1.0, 0.0],
                [1.0, 0.0],
            ],
        }
    }

    pub fn query(model_id: &str) -> FixtureTower<'_> {
        FixtureTower {
            role: 1,
            model_id,
            prefix: "query: ",
            architecture: 1,
            pooling: 1,
            dims: 2,
            word_vectors: [
                [0.0, 0.0],
                [0.0, 0.0],
                [1.0, 0.0],
                [0.0, 0.0],
                [0.0, 0.0],
                [1.0, 0.0],
                [1.0, 0.0],
            ],
        }
    }
}

struct Tensor {
    name: String,
    shape: Vec<u32>,
    bytes: Vec<u8>,
}

pub fn write_symmetric_fixture_bundle(path: &Path) -> EpochIdentity {
    write_fixture_bundle(path, &[FixtureTower::document("fixture-symmetric")], &[])
}

pub fn write_fixture_bundle(
    path: &Path,
    towers: &[FixtureTower<'_>],
    alignment: &[u8],
) -> EpochIdentity {
    write_fixture_bytes(path, towers, alignment);
    fixture_epoch(path)
}

pub fn write_unchecked_fixture_bundle(path: &Path, towers: &[FixtureTower<'_>], alignment: &[u8]) {
    write_fixture_bytes(path, towers, alignment);
}

fn write_fixture_bytes(path: &Path, towers: &[FixtureTower<'_>], alignment: &[u8]) {
    let mut tensors = Vec::new();
    for tower in towers {
        let prefix = if tower.role == 0 { "document" } else { "query" };
        tensors.push(Tensor {
            name: format!("{prefix}/embeddings.word_embeddings.weight"),
            shape: vec![7, 2],
            bytes: f32_bytes(&tower.word_vectors.concat()),
        });
        tensors.push(Tensor {
            name: format!("{prefix}/embeddings.position_embeddings.weight"),
            shape: vec![16, 2],
            bytes: f32_bytes(&[0.0; 32]),
        });
        tensors.push(Tensor {
            name: format!("{prefix}/embeddings.LayerNorm.weight"),
            shape: vec![2],
            bytes: f32_bytes(&[1.0, 1.0]),
        });
        tensors.push(Tensor {
            name: format!("{prefix}/embeddings.LayerNorm.bias"),
            shape: vec![2],
            bytes: f32_bytes(&[0.0, 0.0]),
        });
    }

    let mut body = Vec::new();
    body.extend_from_slice(b"ZEMB0001");
    push_u32(&mut body, 1);
    push_u32(&mut body, HEADER_LEN as u32);
    body.push(u8::try_from(towers.len()).expect("fixture tower count fits u8"));
    body.extend_from_slice(&[0; 7]);
    for tower in towers {
        write_tower(&mut body, tower, &tensors);
    }
    write_tokenizer(&mut body);
    push_f64(&mut body, 0.5);
    push_u32(
        &mut body,
        u32::try_from(tensors.len()).expect("fixture tensor count fits u32"),
    );
    let mut offset = HEADER_LEN;
    for tensor in &tensors {
        push_string(&mut body, &tensor.name);
        body.push(0);
        body.push(u8::try_from(tensor.shape.len()).expect("fixture rank fits u8"));
        body.extend_from_slice(&[0; 2]);
        for dimension in &tensor.shape {
            push_u32(&mut body, *dimension);
        }
        push_u64(&mut body, offset as u64);
        push_u64(&mut body, tensor.bytes.len() as u64);
        push_u64(&mut body, xxhash_rust::xxh3::xxh3_64(&tensor.bytes));
        offset += tensor.bytes.len();
    }
    push_bytes(&mut body, alignment);
    assert!(
        body.len() <= HEADER_LEN,
        "fixture header exceeds reserved bytes"
    );
    body.resize(HEADER_LEN, 0);
    for tensor in &tensors {
        body.extend_from_slice(&tensor.bytes);
    }
    let digest = xxhash_rust::xxh3::xxh3_128(&body);
    body.extend_from_slice(&digest.to_le_bytes());
    std::fs::write(path, body).expect("write fixture bundle");
}

fn fixture_epoch(path: &Path) -> EpochIdentity {
    let bundle = Bundle::open(path).expect("open fixture bundle");
    let tokenizer = Analyzer::new(TokenizerConfig::text_default()).expect("fixture analyzer");
    StoreEpoch {
        embedding: EmbeddingEpoch {
            document: bundle.document_tower().embedding.clone(),
            query: bundle.query_tower().embedding.clone(),
            alignment_digest: bundle.alignment_digest().to_vec(),
        },
        tokenizer: tokenizer.epoch(),
    }
    .identity()
}

fn write_tower(output: &mut Vec<u8>, tower: &FixtureTower<'_>, tensors: &[Tensor]) {
    output.push(tower.role);
    push_string(output, tower.model_id);
    push_string(output, "fixture-v1");
    let tensor_prefix = if tower.role == 0 {
        "document/"
    } else {
        "query/"
    };
    let mut exact_weights = Vec::new();
    for tensor in tensors
        .iter()
        .filter(|tensor| tensor.name.starts_with(tensor_prefix))
    {
        exact_weights.extend_from_slice(&tensor.bytes);
    }
    push_bytes(
        output,
        &xxhash_rust::xxh3::xxh3_128(&exact_weights).to_le_bytes(),
    );
    push_u32(output, tower.dims);
    push_u16(output, 1);
    push_string(output, tower.prefix);
    push_u32(output, 16);
    push_u16(output, 2);
    push_u16(output, 2);
    output.push(0);
    output.push(tower.pooling);
    push_u16(output, tower.architecture);
    push_u16(output, 0);
    push_u32(output, 2);
    push_u16(output, 1);
    push_u32(output, 2);
    push_u32(output, 0);
    push_u32(output, 16);
    push_f32(output, 1.0e-5);
    push_f32(output, 10_000.0);
    push_u32(output, 0);
}

fn write_tokenizer(output: &mut Vec<u8>) {
    output.push(1);
    output.push(1);
    output.extend_from_slice(&[0; 2]);
    push_u32(output, 0);
    push_u32(output, 1);
    push_u32(output, 2);
    push_u32(output, 3);
    let vocabulary = [
        "[PAD]", "[UNK]", "[CLS]", "[SEP]", "the", "bronze", "zeppelin",
    ];
    push_u32(output, vocabulary.len() as u32);
    for token in vocabulary {
        push_string(output, token);
        push_f32(output, 0.0);
    }
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn push_bytes(output: &mut Vec<u8>, value: &[u8]) {
    push_u32(output, value.len() as u32);
    output.extend_from_slice(value);
}

fn push_string(output: &mut Vec<u8>, value: &str) {
    push_bytes(output, value.as_bytes());
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_f32(output: &mut Vec<u8>, value: f32) {
    push_u32(output, value.to_bits());
}

fn push_f64(output: &mut Vec<u8>, value: f64) {
    push_u64(output, value.to_bits());
}
