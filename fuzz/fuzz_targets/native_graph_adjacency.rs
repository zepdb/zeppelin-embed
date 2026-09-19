#![no_main]
use libfuzzer_sys::fuzz_target;
use zeppelin_embed::property_graph::storage::adjacency::{
    self as a, Direction, Edge, RangeKey, UpperBound,
};
use zeppelin_embed::property_graph::{NodeId, RelId, catalog::RelTypeId};
fuzz_target!(|data: &[u8]| {
    if data.len() > a::HEADER_BYTES + a::MAX_BASE_ENTRIES * 40 {
        return;
    }
    let key = RangeKey {
        node: NodeId::new(1 << 100).unwrap(),
        rel_type: RelTypeId::new(7).unwrap(),
        direction: Direction::Out,
        lower: RelId::new(1).unwrap(),
        upper: UpperBound::Infinity,
    };
    let sentinel = Edge {
        rel: RelId::new(1).unwrap(),
        neighbor: NodeId::new(1).unwrap(),
    };
    let mut output = vec![sentinel; a::MAX_MERGED_ENTRIES];
    let base =
        include_bytes!("../../crates/zeppelin-embed/tests/fixtures/graph-adjacency/base-v1.bin");
    for (input, deltas, watermark) in [(data, Vec::new(), 4), (base.as_slice(), vec![data], 4)] {
        if let Ok(result) = a::merge(
            key,
            watermark,
            u64::MAX,
            input,
            &deltas,
            &mut output,
            &mut |_| Ok::<_, ()>(()),
        ) {
            let mut count = 0;
            for part in result.partitions() {
                let edges = &result.edges()[part.start..part.end];
                assert!(edges.len() <= 4096);
                assert!(!edges.is_empty());
                let mut bytes = vec![0; a::HEADER_BYTES + edges.len() * 32];
                a::encode_base(part.key, result.watermark(), edges, &mut bytes, &mut |_| {
                    Ok::<_, ()>(())
                })
                .unwrap();
                let mut again = vec![sentinel; 4096];
                let round = a::merge(
                    part.key,
                    result.watermark(),
                    result.watermark(),
                    &bytes,
                    &[],
                    &mut again,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
                assert_eq!(round.edges(), edges);
                count += edges.len();
            }
            assert_eq!(count, result.edges().len());
        }
    }
});
