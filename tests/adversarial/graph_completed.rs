//! Completed-owner component probe using actual reservations and copied arrays.
//! Controlled logical input is not native GraphStore/query/lifecycle acceptance.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use zeppelin_embed::property_graph::query::completed::*;
use zeppelin_embed::property_graph::query::resources::QueryMemory;
use zeppelin_embed::property_graph::query::runtime::*;
use zeppelin_embed::property_graph::query::{QueryError, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::{GraphGeneration, GraphRevision, NodeId, StoreInstanceId};

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.completed.bits-and-bags",
    "property-graph.completed.full-id",
    "property-graph.completed.oracle.can-fire",
    "property-graph.completed.copy-limit.fire",
    "property-graph.completed.cancel.fire",
    "property-graph.completed.same-seed-control",
    "property-graph.completed.release",
];
#[derive(Debug, Default)]
pub struct Report {
    pub comparisons: usize,
    pub fires: usize,
    pub controls: usize,
}
struct View {
    token: QueryView,
    lease: SnapshotLease,
}
impl RetainedView for View {
    fn query_view(&self) -> &QueryView {
        &self.token
    }
    fn check_active(&self) -> Result<(), QueryError> {
        self.lease
            .check_active()
            .map_err(|_| QueryError::ReadCancelled)
    }
}
struct Source<'a> {
    input: ResultInput<'a>,
    cancel: Option<&'a CancelToken>,
}
impl ResultSource for Source<'_> {
    fn result_input(&self) -> Result<ResultInput<'_>, SourceError> {
        if let Some(token) = self.cancel {
            token.cancel();
        }
        Ok(self.input)
    }
}
// The comparator uses primitive complete row tuples, without native equality,
// validators, ID helpers or result codecs in its expected-value calculation.
type Row = (u128, u64, Vec<u8>);
fn check(want: &[Row], got: &[Row]) -> Result<(), String> {
    if want == got {
        Ok(())
    } else {
        Err("completed primitive row discrepancy".into())
    }
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<Report, String> {
    let mut rng = super::test_support::seeded_rng("graph::completed", seed);
    let id = (1_u128 << 100) | u128::from(rng.next_u64());
    let bits = 0x7ff8_0000_0000_0000 | (rng.next_u64() & 0x0007_ffff_ffff_ffff);
    let expected = vec![(id, bits, vec![0, b'x']); 2];
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(8 * 1024 * 1024),
    )
    .map_err(|e| e.to_string())?;
    let shared = GraphResources::from_store(&store).map_err(|e| e.to_string())?;
    let baseline = shared.reserved_bytes().map_err(|e| e.to_string())?;
    let mut report = Report::default();
    for mode in [0, 1, 0, 2, 0] {
        let memory = QueryMemory::new(&shared, 1024 * 1024).map_err(|e| e.to_string())?;
        let view = View {
            token: QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(7)),
            lease: store.snapshot().map_err(|e| e.to_string())?,
        };
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let limits = if mode == 1 {
            RuntimeLimits::default()
                .with_limit(
                    WorkKind::CopiedBytes,
                    (3 * std::mem::size_of::<Value>()) as u64,
                )
                .map_err(|e| e.to_string())?
        } else {
            RuntimeLimits::default()
        };
        let mut context =
            RuntimeContext::new(&view, &control, &memory, limits).map_err(|e| e.to_string())?;
        let before = memory.reserved_bytes();
        let nodes = [Node {
            id: NodeId::new(id).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            generation: GraphGeneration::new(7),
            key: None,
            labels: Span::default(),
            properties: Span::default(),
            text: None,
            vector: None,
        }];
        let values = [
            Value::Node(0),
            Value::F64(bits),
            Value::String(Span::new(1, 2)),
        ];
        let columns = [Column {
            name: Span::new(0, 1),
            kinds: ValueKinds::ANY,
        }; 3];
        let cells = [
            ValueIndex(0),
            ValueIndex(1),
            ValueIndex(2),
            ValueIndex(0),
            ValueIndex(1),
            ValueIndex(2),
        ];
        let source = Source {
            input: ResultInput {
                view: &view.token,
                rows: 2,
                outcome: Outcome::Read,
                pools: Pools {
                    bytes: b"c\0x",
                    values: &values,
                    columns: &columns,
                    cells: &cells,
                    nodes: &nodes,
                    ..Pools::default()
                },
            },
            cancel: (mode == 2).then_some(&token),
        };
        let result = PreparedGraphResult::copy_from(&source, &mut context);
        match mode {
            1 => {
                if !matches!(
                    result,
                    Err(CompletedError::Runtime(RuntimeError::Limit(
                        WorkKind::CopiedBytes
                    )))
                ) {
                    return Err("completed copy-limit did not fire".into());
                }
                if context.counters().get(WorkKind::CopiedBytes)
                    != (3 * std::mem::size_of::<Value>()) as u64
                {
                    return Err("completed failed-copy counter differs".into());
                }
                report.fires += 1;
                coverage.hit(REQUIRED_COVERAGE[3]);
            }
            2 => {
                if !matches!(
                    result,
                    Err(CompletedError::Runtime(RuntimeError::Value(
                        QueryError::Cancelled
                    )))
                ) {
                    return Err("completed source cancellation did not fire".into());
                }
                report.fires += 1;
                coverage.hit(REQUIRED_COVERAGE[4]);
            }
            _ => {
                let owned = result
                    .map_err(|e| format!("completed copy {e:?}"))?
                    .detach(context.counters(), memory.peak_reserved_bytes());
                let mut actual = Vec::new();
                for row in 0..2 {
                    let Some(Value::Node(index)) = owned.cell(row, 0) else {
                        return Err("completed node cell lost".into());
                    };
                    let Some(Value::F64(bits)) = owned.cell(row, 1) else {
                        return Err("completed scalar bits lost".into());
                    };
                    let Some(Value::String(span)) = owned.cell(row, 2) else {
                        return Err("completed string lost".into());
                    };
                    actual.push((
                        owned.pools().nodes[*index as usize].id.get(),
                        *bits,
                        owned
                            .string(*span)
                            .ok_or("completed UTF8 lost")?
                            .as_bytes()
                            .to_vec(),
                    ));
                }
                check(&expected, &actual)?;
                let mut narrowed = actual.clone();
                narrowed[0].0 = u128::from(narrowed[0].0 as u64);
                let mut changed_bits = actual.clone();
                changed_bits[0].1 ^= 1;
                if check(&expected, &narrowed).is_ok()
                    || check(&expected, &changed_bits).is_ok()
                    || check(&expected, &actual[..1]).is_ok()
                {
                    return Err("completed discrepancy control missed".into());
                }
                report.comparisons += 4;
                report.controls += 1;
                for index in [0, 1, 2, 5] {
                    coverage.hit(REQUIRED_COVERAGE[index]);
                }
            }
        }
        if memory.reserved_bytes() != before {
            return Err("completed reservation leaked".into());
        }
        coverage.hit(REQUIRED_COVERAGE[6]);
    }
    if shared.reserved_bytes().map_err(|e| e.to_string())? != baseline {
        return Err("completed aggregate leaked".into());
    }
    store.close().map_err(|e| e.to_string())?;
    Ok(report)
}
