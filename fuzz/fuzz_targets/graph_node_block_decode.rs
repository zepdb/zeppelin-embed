#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::graph::block::{
    GraphNodeBlockBuild, GraphNodeBlockInput, decode_node_blocks, encode_node_blocks,
};

fuzz_target!(|data: &[u8]| {
    if let Ok(decoded) = decode_node_blocks(data) {
        let blocks = (0..decoded.node_count())
            .map(|node_id| {
                decoded
                    .block(node_id)
                    .expect("every node below validated node_count must remain accessible")
            })
            .collect::<Vec<_>>();
        let neighbor_rows = blocks
            .iter()
            .map(|block| {
                block
                    .neighbors_padded()
                    .take(usize::from(block.degree()))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let inputs = blocks
            .iter()
            .zip(&neighbor_rows)
            .map(|(block, neighbors)| GraphNodeBlockInput {
                codes: block.codes(),
                factors: block.factors(),
                flags: block.flags(),
                neighbors,
            })
            .collect::<Vec<_>>();
        let reencoded = encode_node_blocks(GraphNodeBlockBuild {
            layout: decoded.layout(),
            nodes: &inputs,
        })
        .expect("every accepted graph region must be re-encodable");
        assert_eq!(
            reencoded.as_bytes(),
            data,
            "accepted graph bytes must have one canonical encoding"
        );
    }
});
