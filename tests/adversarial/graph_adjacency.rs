//! PG12 real adjacency codec/merge with primitive model and paired controls.
//! No native directory, view, OUT/IN publication or reopen acceptance is claimed.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use zeppelin_embed::property_graph::storage::adjacency::{
    self as a, Action, DeltaEntry, Direction, Edge, Error, RangeKey, UpperBound, Work,
};
use zeppelin_embed::property_graph::{NodeId, RelId, catalog::RelTypeId};
use zeppelin_embed_adversarial_oracle::graph_adjacency as oracle;
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.adjacency.merge",
    "property-graph.adjacency.full-id",
    "property-graph.adjacency.model-paired",
    "property-graph.adjacency.oracle.can-fire",
    "property-graph.adjacency.corruption.fire",
    "property-graph.adjacency.cancel.fire",
    "property-graph.adjacency.budget.fire",
    "property-graph.adjacency.same-seed-control",
];
#[derive(Default, Debug)]
pub struct Report {
    pub comparisons: usize,
    pub fault_fires: usize,
    pub clean_controls: usize,
}
fn key(node: u128, direction: Direction) -> RangeKey {
    RangeKey {
        node: NodeId::new(node).unwrap(),
        rel_type: RelTypeId::new(7).unwrap(),
        direction,
        lower: RelId::new(1).unwrap(),
        upper: UpperBound::Infinity,
    }
}
fn edge(rel: u128, neighbor: u128) -> Edge {
    Edge {
        rel: RelId::new(rel).unwrap(),
        neighbor: NodeId::new(neighbor).unwrap(),
    }
}
fn clean(_: Work) -> Result<(), u8> {
    Ok(())
}
fn base(k: RangeKey, entries: &[Edge]) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; 96 + 32 * entries.len()];
    a::encode_base(k, 0, entries, &mut bytes, &mut clean)
        .map_err(|e| format!("PG12 base {e:?}"))?;
    Ok(bytes)
}
fn units(w: Work) -> usize {
    match w {
        Work::HeaderBytes(n) | Work::EntryBytes(n) | Work::CopyBytes(n) => n,
        Work::Compare | Work::Finish => 1,
        Work::MergeRun => 0, // run cardinality is a separate native read counter
    }
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<Report, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::adjacency_probe", seed);
    let high = u128::from(rng.next_u64()) << 64;
    let values: Vec<_> = (1..=32_u128).map(|r| edge(high + r, 100 + r)).collect();
    let b = base(key(1, Direction::Out), &values)?;
    let mut encoded = Vec::new();
    let mut model = Vec::new();
    for seq in 1..=3_u64 {
        let entries: Vec<_> = (1..=48_u128)
            .filter(|r| r % 3 == u128::from(seq - 1) || *r == 1)
            .map(|r| {
                let insert = r != 1 && rng.next_u32().is_multiple_of(2);
                oracle::Observation {
                    rel: high + r,
                    neighbor: 100 + r,
                    insert,
                }
            })
            .collect();
        let typed: Vec<_> = entries
            .iter()
            .map(|v| DeltaEntry {
                edge: edge(v.rel, v.neighbor),
                action: if v.insert {
                    Action::Insert
                } else {
                    Action::Delete
                },
            })
            .collect();
        let mut bytes = vec![0; 96 + 40 * typed.len()];
        a::encode_delta(key(1, Direction::Out), seq, &typed, &mut bytes, &mut clean)
            .map_err(|e| format!("PG12 delta {e:?}"))?;
        encoded.push(bytes);
        model.push((seq, entries));
    }
    let input: Vec<_> = encoded.iter().map(Vec::as_slice).collect();
    let primitive: Vec<_> = values
        .iter()
        .map(|v| (v.rel.get(), v.neighbor.get()))
        .collect();
    let want = oracle::expected(&primitive, &model)?;
    let mut output = [edge(1, 1); 96];
    let mut events = Vec::new();
    let merged = a::merge(
        key(1, Direction::Out),
        0,
        3,
        &b,
        &input,
        &mut output,
        &mut |w| {
            events.push(w);
            Ok::<_, u8>(())
        },
    )
    .map_err(|e| format!("PG12 merge {e:?}"))?;
    let observed: Vec<_> = merged
        .edges()
        .iter()
        .map(|v| (v.rel.get(), v.neighbor.get()))
        .collect();
    oracle::check(&want, &observed)?;
    let mut report = Report {
        comparisons: 1,
        ..Report::default()
    };
    coverage.hit(REQUIRED_COVERAGE[0]);
    coverage.hit(REQUIRED_COVERAGE[1]);
    let mut ignored_delete = observed.clone();
    ignored_delete.push((high + 1, 101));
    ignored_delete.sort_unstable();
    if oracle::check(&want, &ignored_delete).is_ok() {
        return Err("PG12 oracle missed ignored delete".into());
    }
    report.comparisons += 1;
    coverage.hit(REQUIRED_COVERAGE[3]);
    let triples = [(1, 1, 1, 7), (2, 1, 2, 7), (3, 1, 2, 7)];
    let mut out_observed = Vec::new();
    let mut in_observed = Vec::new();
    for (direction, sink) in [
        (Direction::Out, &mut out_observed),
        (Direction::In, &mut in_observed),
    ] {
        for node in [1, 2] {
            let group: Vec<_> = triples
                .iter()
                .filter_map(|&(r, s, t, _)| match direction {
                    Direction::Out if s == node => Some(edge(r, t)),
                    Direction::In if t == node => Some(edge(r, s)),
                    _ => None,
                })
                .collect();
            let k = key(node, direction);
            let bytes = base(k, &group)?;
            let result = a::merge(k, 0, 0, &bytes, &[], &mut output, &mut clean)
                .map_err(|e| format!("PG12 paired model {e:?}"))?;
            for v in result.edges() {
                sink.push(match direction {
                    Direction::Out => (v.rel.get(), node, v.neighbor.get(), 7),
                    Direction::In => (v.rel.get(), v.neighbor.get(), node, 7),
                });
            }
        }
    }
    oracle::check_paired(&triples, &out_observed, &in_observed)?;
    if oracle::check_paired(&triples, &out_observed, &in_observed[..2]).is_ok() {
        return Err("PG12 comparator missed reverse".into());
    }
    report.comparisons += 2;
    coverage.hit(REQUIRED_COVERAGE[2]);
    coverage.hit(REQUIRED_COVERAGE[3]);
    for stop in [1, events.len() / 2, events.len()] {
        let mut count = 0;
        let result = a::merge(
            key(1, Direction::Out),
            0,
            3,
            &b,
            &input,
            &mut output,
            &mut |_| {
                count += 1;
                if count == stop { Err(1_u8) } else { Ok(()) }
            },
        );
        if !matches!(result, Err(Error::Control(1))) || count != stop {
            return Err("PG12 cancel did not fire at actual work".into());
        }
        report.fault_fires += 1;
        coverage.hit(REQUIRED_COVERAGE[5]);
        a::merge(
            key(1, Direction::Out),
            0,
            3,
            &b,
            &input,
            &mut output,
            &mut clean,
        )
        .map_err(|e| format!("PG12 same-seed cancel control {e:?}"))?;
        report.clean_controls += 1;
        coverage.hit(REQUIRED_COVERAGE[7]);
    }
    let budget = events.iter().copied().map(units).sum::<usize>() - 1;
    let mut work = 0;
    if !matches!(
        a::merge(
            key(1, Direction::Out),
            0,
            3,
            &b,
            &input,
            &mut output,
            &mut |w| {
                work += units(w);
                if work > budget { Err(2_u8) } else { Ok(()) }
            }
        ),
        Err(Error::Control(2))
    ) {
        return Err("PG12 work refusal did not fire".into());
    }
    report.fault_fires += 1;
    coverage.hit(REQUIRED_COVERAGE[6]);
    let mut corrupt = encoded[2].clone();
    *corrupt.last_mut().unwrap() = 1;
    if !matches!(
        a::merge(
            key(1, Direction::Out),
            0,
            3,
            &b,
            &[&encoded[0], &encoded[1], &corrupt],
            &mut output,
            &mut clean
        ),
        Err(Error::Format(_))
    ) {
        return Err("PG12 corrupt final reserved byte accepted".into());
    }
    report.fault_fires += 1;
    coverage.hit(REQUIRED_COVERAGE[4]);
    for _ in 0..2 {
        let result = a::merge(
            key(1, Direction::Out),
            0,
            3,
            &b,
            &input,
            &mut output,
            &mut clean,
        )
        .map_err(|e| format!("PG12 same-seed control {e:?}"))?;
        let observed: Vec<_> = result
            .edges()
            .iter()
            .map(|v| (v.rel.get(), v.neighbor.get()))
            .collect();
        oracle::check(&want, &observed)?;
        report.clean_controls += 1;
        coverage.hit(REQUIRED_COVERAGE[7]);
    }
    Ok(report)
}
