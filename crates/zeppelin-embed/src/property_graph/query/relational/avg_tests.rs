#![allow(clippy::expect_used, clippy::panic)]

use super::*;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{RetainedView, RuntimeLimits};
use crate::property_graph::query::{QueryError, QueryView};
use crate::property_graph::resources::GraphResources;
use crate::property_graph::{GraphGeneration, StoreInstanceId};

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

fn fixture(f: impl FnOnce(&mut RuntimeContext<'_, '_, '_>)) {
    let directory = tempfile::tempdir().expect("directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(64 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("resources");
    let memory = QueryMemory::new(&shared, 1024 * 1024).expect("memory");
    let view = View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("id"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("lease"),
    };
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).expect("context");
    f(&mut context);
}

fn average(
    values: &[QueryValue<'_>],
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<f64, RuntimeError> {
    let capacity = StorageCapacity::new(values.len().max(1), 1024, ArenaCapacity::default());
    let mut rows = Rows::new(context, &[SlotId(0)], capacity)?;
    for value in values {
        rows.push(&[*value], context)?;
    }
    let columns = [AggregateColumn {
        output: SlotId(1),
        operation: Aggregate::Avg {
            slot: SlotId(0),
            distinct: false,
        },
    }];
    let result = rows.aggregate(&[], &columns, capacity, context)?;
    match result.value(0, 0) {
        Some(QueryValue::F64(value)) => Ok(value),
        _ => Err(RuntimeError::Batch),
    }
}

#[test]
fn ze452_avg_divides_integer_and_float_sums_once() {
    fixture(|context| {
        let integers = [1, 48, 87, 27, 54].map(QueryValue::I64);
        assert_eq!(average(&integers, context).expect("average"), 43.4);
        let floats = [1.0, 48.0, 87.0, 27.0, 54.0].map(QueryValue::F64);
        assert_eq!(average(&floats, context).expect("average"), 43.4);
        let widened = [i64::MAX, 1, -i64::MAX].map(QueryValue::I64);
        assert_eq!(average(&widened, context).expect("average"), 1.0 / 3.0);
    });
}

#[test]
fn ze452_avg_small_integers_match_sum_count() {
    use rand::Rng;
    let mut rng = crate::test_support::seeded_rng(
        "property_graph::query::relational::avg_tests::ze452_avg_small_integers_match_sum_count",
    );
    fixture(|context| {
        for case in 0..2000 {
            let values: Vec<i64> = (0..rng.random_range(1..=32))
                .map(|_| rng.random_range(-100..=100))
                .collect();
            let expected = values.iter().sum::<i64>() as f64 / values.len() as f64;
            let operands: Vec<_> = values.iter().copied().map(QueryValue::I64).collect();
            assert_eq!(
                average(&operands, context).expect("average"),
                expected,
                "case {case}: {values:?}"
            );
        }
    });
}

#[test]
fn ze452_avg_nonfinite_inputs_are_arithmetic_domain() {
    fixture(|context| {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for values in [
                vec![QueryValue::F64(value)],
                vec![QueryValue::I64(1), QueryValue::F64(value)],
                vec![
                    QueryValue::F64(1.0e308),
                    QueryValue::F64(1.0e308),
                    QueryValue::F64(value),
                ],
            ] {
                assert!(
                    matches!(
                        average(&values, context),
                        Err(RuntimeError::Value(QueryError::ArithmeticDomain))
                    ),
                    "nonfinite operand {value:?}"
                );
            }
        }
    });
}
