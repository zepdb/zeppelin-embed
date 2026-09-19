#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::query::{
    Comparison, QueryError, QueryList, QueryValue, QueryView, Truth, ValueContext,
};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
fn view() -> QueryView {
    QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(7))
}
fn compare(
    left: QueryValue<'_>,
    right: QueryValue<'_>,
    op: Comparison,
) -> Result<Truth, QueryError> {
    let view = view();
    let control = QueryControl::Cancel(CancelToken::new());
    left.predicate(
        right,
        op,
        &mut ValueContext::new(&view, &control, 8_000_000).unwrap(),
    )
}

#[test]
fn query_boolean_truth_tables_preserve_unknown() {
    use Truth::{False as F, True as T, Unknown as U};
    // Literal tables from the pinned comparison CIP, not production formulas.
    let cases = [
        (T, T, T, T, F),
        (T, U, U, T, U),
        (T, F, F, T, T),
        (U, T, U, T, U),
        (U, U, U, U, U),
        (U, F, F, U, U),
        (F, T, F, T, T),
        (F, U, F, U, U),
        (F, F, F, F, F),
    ];
    for (left, right, and, or, xor) in cases {
        assert_eq!(left.and(right), and);
        assert_eq!(left.or(right), or);
        assert_eq!(left.xor(right), xor);
    }
    assert_eq!([T.not(), U.not(), F.not()], [F, U, T]);
    assert_eq!(
        [T.retained(), U.retained(), F.retained()],
        [true, false, false]
    );
    assert_eq!(QueryValue::Null.truth(), Ok(U));
    assert_eq!(QueryValue::Bool(true).truth(), Ok(T));
    assert_eq!(QueryValue::Bool(false).truth(), Ok(F));
    assert_eq!(QueryValue::I64(1).truth(), Err(QueryError::Type));
}

#[test]
fn mixed_numeric_predicates_never_round_integer_identity() {
    use QueryValue::{F64 as F, I64 as I};
    use Truth::{False, True, Unknown};
    use zeppelin_embed::property_graph::query::Comparison::{Equal, Greater, Less};
    let cases = [
        (I(1), F(1.0), Equal, True),
        (
            I(9_007_199_254_740_993),
            F(9_007_199_254_740_992.0),
            Equal,
            False,
        ),
        (
            I(9_007_199_254_740_993),
            F(9_007_199_254_740_992.0),
            Greater,
            True,
        ),
        (I(i64::MAX), F(9_223_372_036_854_775_808.0), Less, True),
        (I(i64::MIN), F(-9_223_372_036_854_775_808.0), Equal, True),
        (
            I(-9_007_199_254_740_993),
            F(-9_007_199_254_740_992.0),
            Less,
            True,
        ),
        (I(0), F(f64::from_bits(1)), Less, True),
        (I(0), F(-f64::from_bits(1)), Greater, True),
        (I(0), F(-0.0), Equal, True),
        (F(0.0), F(-0.0), Equal, True),
        (I(2), F(2.5), Less, True),
        (I(-2), F(-2.5), Greater, True),
        (I(i64::MAX), F(f64::INFINITY), Less, True),
        (I(i64::MIN), F(f64::NEG_INFINITY), Greater, True),
        (F(f64::INFINITY), F(f64::INFINITY), Equal, True),
        (F(f64::NAN), F(f64::NAN), Equal, False),
        (F(f64::NAN), F(f64::INFINITY), Greater, False),
        (I(0), QueryValue::Null, Equal, Unknown),
        (QueryValue::Bool(false), I(0), Equal, False),
        (QueryValue::Bool(false), I(0), Less, Unknown),
    ];
    for (left, right, operation, expected) in cases {
        assert_eq!(compare(left, right, operation), Ok(expected));
    }
    use zeppelin_embed::property_graph::query::Comparison::{GreaterEqual, LessEqual, NotEqual};
    for value in [I(0), F(f64::INFINITY), F(f64::NAN)] {
        for operation in [Equal, Less, LessEqual, Greater, GreaterEqual] {
            assert_eq!(compare(F(f64::NAN), value, operation), Ok(False));
            assert_eq!(compare(value, F(f64::NAN), operation), Ok(False));
        }
        assert_eq!(compare(F(f64::NAN), value, NotEqual), Ok(True));
    }
}

#[test]
fn arithmetic_rejects_domain_overflow_and_zero_divisors() {
    use QueryValue::{F64 as F, I64 as I};
    use zeppelin_embed::property_graph::query::Arithmetic::{
        Add, Divide, Multiply, Remainder, Subtract,
    };
    let failures = [
        (I(i64::MAX), I(1), Add, QueryError::ArithmeticOverflow),
        (I(i64::MIN), I(1), Subtract, QueryError::ArithmeticOverflow),
        (I(i64::MAX), I(2), Multiply, QueryError::ArithmeticOverflow),
        (I(i64::MIN), I(-1), Divide, QueryError::ArithmeticOverflow),
        (
            I(i64::MIN),
            I(-1),
            Remainder,
            QueryError::ArithmeticOverflow,
        ),
        (I(1), I(0), Divide, QueryError::DivisionByZero),
        (F(1.0), F(-0.0), Divide, QueryError::DivisionByZero),
        (I(1), F(0.0), Remainder, QueryError::DivisionByZero),
        (F(f64::INFINITY), I(1), Add, QueryError::ArithmeticDomain),
        (I(1), F(f64::NAN), Multiply, QueryError::ArithmeticDomain),
        (
            F(f64::MAX),
            F(2.0),
            Multiply,
            QueryError::ArithmeticOverflow,
        ),
    ];
    for (left, right, operation, expected) in failures {
        assert!(matches!(left.arithmetic(right, operation), Err(error) if error == expected));
    }
    assert!(matches!(
        I(i64::MIN).negate(),
        Err(QueryError::ArithmeticOverflow)
    ));
    assert!(matches!(
        F(f64::INFINITY).negate(),
        Err(QueryError::ArithmeticDomain)
    ));
    assert!(matches!(I(7).arithmetic(I(2), Divide), Ok(I(3))));
    assert!(matches!(I(-7).arithmetic(I(2), Remainder), Ok(I(-1))));
    assert!(
        matches!(I(9_007_199_254_740_993).arithmetic(F(0.0), Add), Ok(F(value)) if value == 9_007_199_254_740_992.0)
    );
    assert!(
        matches!(F(-f64::MIN_POSITIVE).arithmetic(F(f64::MAX), Divide), Ok(F(value)) if value.to_bits() == (-0.0_f64).to_bits())
    );
    assert!(matches!(
        QueryValue::Null.arithmetic(I(1), Add),
        Ok(QueryValue::Null)
    ));
    assert!(matches!(
        QueryValue::Bool(true).arithmetic(I(1), Add),
        Err(QueryError::Type)
    ));
}

#[test]
fn nested_list_predicates_preserve_unknown_and_false_dominance() {
    use Comparison::{Equal, GreaterEqual, Less};
    use Truth::{False, True, Unknown};
    let view = view();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let nullable = [QueryValue::Null, QueryValue::I64(1)];
    let mismatch = [QueryValue::I64(1), QueryValue::I64(2)];
    let first = QueryValue::List(QueryList::new(&nullable, &mut context).unwrap());
    let second = QueryValue::List(QueryList::new(&mismatch, &mut context).unwrap());
    assert_eq!(first.predicate(first, Equal, &mut context), Ok(Unknown));
    assert_eq!(first.predicate(second, Equal, &mut context), Ok(False));
    let nested_first = [first];
    let nested_second = [second];
    let a = QueryValue::List(QueryList::new(&nested_first, &mut context).unwrap());
    let b = QueryValue::List(QueryList::new(&nested_second, &mut context).unwrap());
    assert_eq!(a.predicate(b, Equal, &mut context), Ok(False));
    assert_eq!(a.predicate(a, Equal, &mut context), Ok(Unknown));
    assert_eq!(QueryValue::I64(1).in_list(first, &mut context), Ok(True));
    assert_eq!(QueryValue::I64(2).in_list(first, &mut context), Ok(Unknown));
    let empty = QueryValue::List(QueryList::new(&[], &mut context).unwrap());
    assert_eq!(QueryValue::Null.in_list(empty, &mut context), Ok(False));
    assert_eq!(
        QueryValue::I64(1).in_list(QueryValue::Null, &mut context),
        Ok(Unknown)
    );
    assert_eq!(empty.predicate(first, Less, &mut context), Ok(True));
    let low = [QueryValue::I64(1), QueryValue::I64(2)];
    let high = [QueryValue::I64(3), QueryValue::Null];
    let low = QueryValue::List(QueryList::new(&low, &mut context).unwrap());
    let high = QueryValue::List(QueryList::new(&high, &mut context).unwrap());
    assert_eq!(low.predicate(high, GreaterEqual, &mut context), Ok(False));
    assert_eq!(
        QueryValue::String("é").predicate(QueryValue::String("z"), Less, &mut context),
        Ok(False)
    );
    assert_eq!(
        QueryValue::String("é").predicate(QueryValue::String("e\u{301}"), Equal, &mut context),
        Ok(False)
    );
    assert!(context.work() > 0);
}

#[test]
fn full_width_entity_references_and_packed_lists_are_view_owned() {
    use zeppelin_embed::property_graph::{NodeId, RelId};
    let view = view();
    let foreign = QueryView::new(view.store(), view.generation());
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let low = NodeId::new(9).unwrap();
    let high = NodeId::new((1 << 100) + 9).unwrap();
    let a = view.node(low);
    let b = view.node(high);
    assert_eq!(
        a.predicate(b, Comparison::Equal, &mut context),
        Ok(Truth::False)
    );
    assert_eq!(
        a.predicate(b, Comparison::Less, &mut context),
        Ok(Truth::True)
    );
    assert_eq!(
        b.predicate(
            view.relationship(RelId::new(high.get()).unwrap()),
            Comparison::Equal,
            &mut context
        ),
        Ok(Truth::False)
    );
    assert_eq!(
        a.predicate(foreign.node(low), Comparison::Equal, &mut context),
        Err(QueryError::ForeignView)
    );
    assert!(matches!(
        QueryList::new(&[foreign.node(low)], &mut context),
        Err(QueryError::ForeignView)
    ));
    let ids = vec![high; 524_288];
    let packed = QueryList::nodes(&view, &ids, &mut context).unwrap();
    assert_eq!(
        (packed.len(), packed.elements(), packed.depth()),
        (524_288, 524_288, 1)
    );
    assert_eq!(packed.borrowed_bytes(), 8 * 1024 * 1024);
    assert!(packed.borrowed_bytes() + std::mem::size_of_val(&packed) <= 8 * 1024 * 1024 + 128);
    assert_eq!(
        packed
            .get(524_287)
            .unwrap()
            .predicate(b, Comparison::Equal, &mut context),
        Ok(Truth::True)
    );
    assert!(packed.get(524_288).is_none());
    let too_many = vec![low; 524_289];
    assert!(matches!(
        QueryList::nodes(&view, &too_many, &mut context),
        Err(QueryError::ListLimit)
    ));
    assert!(matches!(
        QueryList::nodes(&foreign, &ids, &mut context),
        Err(QueryError::ForeignView)
    ));
    let rels = [RelId::new(9).unwrap(), RelId::new((1 << 96) + 9).unwrap()];
    let packed_rels = QueryList::relationships(&view, &rels, &mut context).unwrap();
    assert_eq!(
        packed_rels.get(0).unwrap().predicate(
            packed_rels.get(1).unwrap(),
            Comparison::Less,
            &mut context
        ),
        Ok(Truth::True)
    );
    let nested = [QueryValue::List(packed)];
    assert!(matches!(
        QueryList::new(&nested, &mut context),
        Err(QueryError::ListLimit)
    ));
}

#[test]
fn grouping_hash_and_total_order_follow_equivalence_not_replay_bits() {
    use std::cmp::Ordering::{Equal, Less};
    let view = view();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let nan_a = QueryValue::F64(f64::from_bits(0x7ff8_0000_0000_0042));
    let nan_b = QueryValue::F64(f64::from_bits(0xfff8_0000_0000_9999));
    for (a, b) in [
        (QueryValue::Null, QueryValue::Null),
        (nan_a, nan_b),
        (QueryValue::I64(0), QueryValue::F64(-0.0)),
        (QueryValue::I64(1), QueryValue::F64(1.0)),
        (
            QueryValue::I64(i64::MIN),
            QueryValue::F64(-9_223_372_036_854_775_808.0),
        ),
    ] {
        assert_eq!(a.equivalent(b, &mut context), Ok(true));
        assert_eq!(a.order(b, &mut context), Ok(Equal));
        assert_eq!(
            a.group_hash(&mut context).unwrap(),
            b.group_hash(&mut context).unwrap()
        );
    }
    let a = [QueryValue::Null, nan_a, QueryValue::F64(-0.0)];
    let b = [QueryValue::Null, nan_b, QueryValue::I64(0)];
    let a = QueryValue::List(QueryList::new(&a, &mut context).unwrap());
    let b = QueryValue::List(QueryList::new(&b, &mut context).unwrap());
    assert_eq!(a.equivalent(b, &mut context), Ok(true));
    assert_eq!(
        a.group_hash(&mut context).unwrap(),
        b.group_hash(&mut context).unwrap()
    );
    let ordered = [
        view.node(zeppelin_embed::property_graph::NodeId::new(u128::MAX).unwrap()),
        view.relationship(zeppelin_embed::property_graph::RelId::new(1).unwrap()),
        QueryValue::List(QueryList::new(&[], &mut context).unwrap()),
        QueryValue::String(""),
        QueryValue::Bool(false),
        QueryValue::Bool(true),
        QueryValue::F64(f64::NEG_INFINITY),
        QueryValue::I64(i64::MIN),
        QueryValue::I64(0),
        QueryValue::I64(9_007_199_254_740_993),
        QueryValue::I64(i64::MAX),
        QueryValue::F64(9_223_372_036_854_775_808.0),
        QueryValue::F64(f64::INFINITY),
        nan_a,
        QueryValue::Null,
    ];
    for pair in ordered.windows(2) {
        assert_eq!(pair[0].order(pair[1], &mut context), Ok(Less));
    }
    assert_eq!(
        QueryValue::I64(9_007_199_254_740_993)
            .equivalent(QueryValue::F64(9_007_199_254_740_992.0), &mut context),
        Ok(false)
    );
    assert_eq!(nan_a.equivalent(QueryValue::Null, &mut context), Ok(false));
}

#[test]
fn property_assignment_validates_whole_list_before_copy_and_preserves_empty_kind() {
    use zeppelin_embed::property_graph::query::PropertyScratch;
    use zeppelin_embed::property_graph::{PropertyData, PropertyValue};
    let view = view();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    assert!(
        QueryValue::Null
            .to_property(PropertyScratch::None, &mut context)
            .unwrap()
            .data()
            .is_none()
    );
    let values = [QueryValue::I64(1), QueryValue::I64(i64::MAX)];
    let list = QueryValue::List(QueryList::new(&values, &mut context).unwrap());
    let mut scratch = [7_i64; 3];
    let converted = list
        .to_property(PropertyScratch::Integers(&mut scratch), &mut context)
        .unwrap();
    assert!(
        matches!(converted.data(), Some(PropertyData::Integers(values)) if values == [1, i64::MAX])
    );
    assert_eq!(scratch, [1, i64::MAX, 7]);
    for values in [
        [QueryValue::I64(1), QueryValue::F64(2.0)],
        [QueryValue::I64(1), QueryValue::Null],
    ] {
        let list = QueryValue::List(QueryList::new(&values, &mut context).unwrap());
        let mut scratch = [77_i64; 2];
        assert!(matches!(
            list.to_property(PropertyScratch::Integers(&mut scratch), &mut context),
            Err(QueryError::Type)
        ));
        assert_eq!(scratch, [77, 77]);
    }
    let empty = QueryValue::List(QueryList::new(&[], &mut context).unwrap());
    assert!(matches!(
        empty
            .to_property(PropertyScratch::None, &mut context)
            .unwrap()
            .data(),
        Some(PropertyData::EmptyList { count: 0 })
    ));
    let typed = PropertyValue::new(PropertyData::Floats(&[])).unwrap();
    let read = QueryValue::from_property(Some(typed), &mut context).unwrap();
    assert_eq!(read.equivalent(empty, &mut context), Ok(true));
    assert!(matches!(typed.data(), PropertyData::Floats(values) if values.is_empty()));
    assert!(matches!(
        QueryValue::from_property(None, &mut context),
        Ok(QueryValue::Null)
    ));
    let nan = f64::from_bits(0xfff8_0000_0000_1234);
    let floats = [QueryValue::F64(nan), QueryValue::F64(-0.0)];
    let list = QueryValue::List(QueryList::new(&floats, &mut context).unwrap());
    let mut output = [0.0; 2];
    let converted = list
        .to_property(PropertyScratch::Floats(&mut output), &mut context)
        .unwrap();
    assert!(
        matches!(converted.data(), Some(PropertyData::Floats(values)) if values[0].to_bits() == nan.to_bits() && values[1].to_bits() == (-0.0_f64).to_bits())
    );
}

#[test]
fn typed_id_text_preserves_all_bits_and_null_without_a_storage_fetch() {
    use zeppelin_embed::property_graph::{NodeId, RelId};
    let view = view();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut output = [b'!'; 32];
    let text = view
        .node(NodeId::new((1 << 64) + 15).unwrap())
        .node_id_text(&mut output, &mut context)
        .unwrap();
    assert!(matches!(
        text,
        QueryValue::String("0000000000000001000000000000000f")
    ));
    assert_eq!(context.produced_bytes(), 32);
    let text = view
        .relationship(RelId::new(u128::MAX).unwrap())
        .relationship_id_text(&mut output, &mut context)
        .unwrap();
    assert!(matches!(
        text,
        QueryValue::String("ffffffffffffffffffffffffffffffff")
    ));
    assert_eq!(context.produced_bytes(), 64);
    assert!(matches!(
        QueryValue::Null.node_id_text(&mut output, &mut context),
        Ok(QueryValue::Null)
    ));
    assert_eq!(output, [b'f'; 32]);
    assert!(matches!(
        view.relationship(RelId::new(1).unwrap())
            .node_id_text(&mut output, &mut context),
        Err(QueryError::Type)
    ));
    let foreign = QueryView::new(view.store(), view.generation());
    assert!(matches!(
        foreign
            .node(NodeId::new(1).unwrap())
            .node_id_text(&mut output, &mut context),
        Err(QueryError::ForeignView)
    ));
}

#[test]
fn bounded_string_and_list_expressions_preserve_unicode_and_null() {
    use zeppelin_embed::property_graph::query::StringPredicate::{Contains, EndsWith, StartsWith};
    let view = view();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let text = QueryValue::String("aé\u{301}\0🙂");
    assert!(matches!(text.size(&mut context), Ok(QueryValue::I64(5))));
    assert_eq!(
        text.string_predicate(QueryValue::String("é\u{301}"), Contains, &mut context),
        Ok(Truth::True)
    );
    assert_eq!(
        text.string_predicate(QueryValue::String("A"), StartsWith, &mut context),
        Ok(Truth::False)
    );
    assert_eq!(
        text.string_predicate(QueryValue::String("🙂"), EndsWith, &mut context),
        Ok(Truth::True)
    );
    assert_eq!(
        text.string_predicate(QueryValue::String(""), Contains, &mut context),
        Ok(Truth::True)
    );
    assert_eq!(
        text.string_predicate(QueryValue::Null, Contains, &mut context),
        Ok(Truth::Unknown)
    );
    assert_eq!(
        text.string_predicate(QueryValue::I64(1), Contains, &mut context),
        Err(QueryError::Type)
    );
    let values = [QueryValue::I64(1), QueryValue::Null, QueryValue::I64(3)];
    let list = QueryValue::List(QueryList::new(&values, &mut context).unwrap());
    assert!(matches!(list.size(&mut context), Ok(QueryValue::I64(3))));
    assert!(matches!(
        list.index(QueryValue::I64(-1), &mut context),
        Ok(QueryValue::I64(3))
    ));
    for index in [3, -4, i64::MIN, i64::MAX] {
        assert!(matches!(
            list.index(QueryValue::I64(index), &mut context),
            Ok(QueryValue::Null)
        ));
    }
    assert!(matches!(
        list.index(QueryValue::F64(1.0), &mut context),
        Err(QueryError::Type)
    ));
    assert!(matches!(
        QueryValue::Null.size(&mut context),
        Ok(QueryValue::Null)
    ));
}

#[test]
fn value_limits_accept_exact_depth_and_stop_before_unreserved_work() {
    fn nested(
        depth: usize,
        context: &mut ValueContext<'_>,
        visit: &mut dyn for<'a> FnMut(
            QueryValue<'a>,
            &mut ValueContext<'_>,
        ) -> Result<(), QueryError>,
    ) -> Result<(), QueryError> {
        if depth == 0 {
            return visit(QueryValue::Null, context);
        }
        nested(depth - 1, context, &mut |child, context| {
            let values = [child];
            let list = QueryList::new(&values, context)?;
            visit(QueryValue::List(list), context)
        })
    }
    let view = view();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    nested(16, &mut context, &mut |value, _| {
        let QueryValue::List(list) = value else {
            panic!("list")
        };
        assert_eq!((list.depth(), list.elements()), (16, 16));
        Ok(())
    })
    .unwrap();
    assert_eq!(
        nested(17, &mut context, &mut |_, _| Ok(())),
        Err(QueryError::ListLimit)
    );
    let mut tight = ValueContext::new(&view, &control, 1).unwrap();
    assert_eq!(
        QueryValue::I64(1).predicate(QueryValue::I64(1), Comparison::Equal, &mut tight),
        Ok(Truth::True)
    );
    assert_eq!(
        QueryValue::I64(1).predicate(QueryValue::I64(1), Comparison::Equal, &mut tight),
        Err(QueryError::WorkLimit)
    );
    assert_eq!(tight.work(), 1);
    assert!(matches!(
        ValueContext::new(&view, &control, 8_000_001),
        Err(QueryError::WorkLimit)
    ));
    let token = CancelToken::new();
    token.cancel();
    assert!(matches!(
        ValueContext::new(&view, &QueryControl::Cancel(token), 8_000_000),
        Err(QueryError::Cancelled)
    ));
}

#[cfg(feature = "test-support")]
#[test]
fn deadline_is_checked_inside_byte_chunks_and_list_elements() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use std::time::{Duration, Instant};
    use zeppelin_embed::lifecycle::{Deadline, MonotonicClock};
    struct Clock {
        base: Instant,
        ticks: AtomicU64,
    }
    impl MonotonicClock for Clock {
        fn now(&self) -> Instant {
            self.base + Duration::from_nanos(self.ticks.fetch_add(1, Ordering::SeqCst))
        }
    }
    fn deadline(ticks: u64) -> QueryControl {
        QueryControl::Deadline(
            Deadline::after_with_test_clock(
                Duration::from_nanos(ticks),
                Arc::new(Clock {
                    base: Instant::now(),
                    ticks: AtomicU64::new(0),
                }),
            )
            .unwrap(),
        )
    }
    let view = view();
    let text = "x".repeat(65_537);
    let control = deadline(4);
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    assert_eq!(
        QueryValue::String(&text).predicate(
            QueryValue::String(&text),
            Comparison::Equal,
            &mut context
        ),
        Err(QueryError::Timeout)
    );
    assert_eq!(context.work(), 2);
    let values = vec![QueryValue::I64(1); 400];
    let control = deadline(300);
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    assert!(matches!(
        QueryList::new(&values, &mut context),
        Err(QueryError::Timeout)
    ));
    assert_eq!(context.work(), 297);
    let control = deadline(1_000);
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    assert_eq!(QueryList::new(&values, &mut context).unwrap().len(), 400);
    assert_eq!(
        QueryValue::String(&text).predicate(
            QueryValue::String(&text),
            Comparison::Equal,
            &mut context
        ),
        Ok(Truth::True)
    );
}
