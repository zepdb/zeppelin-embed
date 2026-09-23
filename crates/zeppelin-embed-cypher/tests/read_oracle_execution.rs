//! ZE-56 local fixtures (not original TCK): an independent tiny-graph oracle.
//! A plain Rust model of five nodes and five relationships computes each
//! expected table with ordinary iterators; the engine answers the same
//! question from Cypher text on a persisted native store, before and after a
//! reopen. The comparison checks nulls, integer versus float types, bag
//! multiplicity and explicit ORDER BY/SKIP/LIMIT order.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
#[path = "support/graph.rs"]
mod graph;
mod support;
#[path = "support/tck.rs"]
mod tck;

use graph::Graph;
use std::cmp::Ordering;
use tck::V;
use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::query::completed::{
    GraphQueryErrorKind, GraphQueryOptions, Outcome,
};
use zeppelin_embed_cypher::{CompileLimits, StatementError, execute};

#[derive(Clone, Copy)]
enum Age {
    Missing,
    Int(i64),
    Float(f64),
}

struct Person {
    name: &'static str,
    label: &'static str,
    age: Age,
}

const PEOPLE: [Person; 5] = [
    Person {
        name: "alice",
        label: "Person",
        age: Age::Int(30),
    },
    Person {
        name: "bob",
        label: "Person",
        age: Age::Int(25),
    },
    Person {
        name: "carol",
        label: "Person",
        age: Age::Missing,
    },
    Person {
        name: "dave",
        label: "Person",
        age: Age::Float(30.5),
    },
    Person {
        name: "eve",
        label: "Robot",
        age: Age::Int(30),
    },
];
/// (source, target) indices into PEOPLE; all typed KNOWS.
const KNOWS: [(usize, usize); 5] = [(0, 1), (0, 2), (1, 2), (3, 2), (4, 0)];

fn setup_text() -> String {
    let nodes: Vec<String> = PEOPLE
        .iter()
        .enumerate()
        .map(|(index, person)| {
            let age = match person.age {
                Age::Missing => String::new(),
                Age::Int(value) => format!(", age: {value}"),
                Age::Float(value) => format!(", age: {value:?}"),
            };
            format!(
                "(n{index}:{} {{name: '{}'{age}}})",
                person.label, person.name
            )
        })
        .collect();
    let edges: Vec<String> = KNOWS
        .iter()
        .map(|(source, target)| format!("(n{source})-[:KNOWS]->(n{target})"))
        .collect();
    format!("CREATE {}, {}", nodes.join(", "), edges.join(", "))
}

fn age(value: Age) -> V {
    match value {
        Age::Missing => V::Null,
        Age::Int(value) => V::Int(value),
        Age::Float(value) => V::Float(value),
    }
}

fn number(value: Age) -> Option<f64> {
    match value {
        Age::Missing => None,
        Age::Int(value) => Some(value as f64),
        Age::Float(value) => Some(value),
    }
}

fn persons() -> impl Iterator<Item = &'static Person> {
    PEOPLE.iter().filter(|person| person.label == "Person")
}

fn name(value: &str) -> V {
    V::Str(value.to_owned())
}

/// One oracle question: its Cypher text, whether row order is part of the
/// answer, whether list element order is, and the model's expected table.
struct Question {
    text: &'static str,
    ordered: bool,
    unordered_lists: bool,
    columns: Vec<&'static str>,
    rows: Vec<Vec<V>>,
}

fn questions() -> Vec<Question> {
    // Cypher orders null after every number ascending, so first descending.
    let mut by_age_desc: Vec<&Person> = persons().collect();
    by_age_desc.sort_by(|a, b| {
        let order = match (number(a.age), number(b.age)) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => y.partial_cmp(&x).unwrap(),
        };
        order.then(a.name.cmp(b.name))
    });
    let mut names: Vec<&str> = persons().map(|person| person.name).collect();
    names.sort();
    let mut distinct_ages: Vec<V> = Vec::new();
    for person in &PEOPLE {
        let value = age(person.age);
        let seen = distinct_ages.iter().any(|known| match (known, &value) {
            (V::Int(x), V::Float(y)) | (V::Float(y), V::Int(x)) => *x as f64 == *y,
            (known, value) => known == value,
        });
        if !seen {
            distinct_ages.push(value);
        }
    }
    let mut friends: Vec<Vec<V>> = persons()
        .map(|person| {
            let index = PEOPLE.iter().position(|p| p.name == person.name).unwrap();
            let known: Vec<V> = KNOWS
                .iter()
                .filter(|(source, _)| *source == index)
                .map(|(_, target)| name(PEOPLE[*target].name))
                .collect();
            vec![
                name(person.name),
                V::Int(known.len() as i64),
                V::List(known),
            ]
        })
        .collect();
    friends.sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    vec![
        Question {
            text: "MATCH (p:Person) RETURN p.name AS name, p.age AS age ORDER BY p.age DESC, p.name",
            ordered: true,
            unordered_lists: false,
            columns: vec!["name", "age"],
            rows: by_age_desc
                .iter()
                .map(|person| vec![name(person.name), age(person.age)])
                .collect(),
        },
        Question {
            text: "MATCH (p:Person) RETURN p.name AS name ORDER BY name SKIP 1 LIMIT 2",
            ordered: true,
            unordered_lists: false,
            columns: vec!["name"],
            rows: names
                .iter()
                .skip(1)
                .take(2)
                .map(|n| vec![name(n)])
                .collect(),
        },
        Question {
            text: "MATCH (a)-[:KNOWS]->(b) RETURN b.name AS name",
            ordered: false,
            unordered_lists: false,
            columns: vec!["name"],
            rows: KNOWS
                .iter()
                .map(|(_, target)| vec![name(PEOPLE[*target].name)])
                .collect(),
        },
        Question {
            text: "MATCH (p) RETURN DISTINCT p.age AS age",
            ordered: false,
            unordered_lists: false,
            columns: vec!["age"],
            rows: distinct_ages.into_iter().map(|value| vec![value]).collect(),
        },
        Question {
            text: "MATCH (p:Person) OPTIONAL MATCH (p)-[:KNOWS]->(f) \
                   RETURN p.name AS name, count(f) AS friends, collect(f.name) AS names \
                   ORDER BY name",
            ordered: true,
            unordered_lists: true,
            columns: vec!["name", "friends", "names"],
            rows: friends,
        },
        Question {
            text: "MATCH (p:Person) WHERE p.age > 25 RETURN p.name AS name",
            ordered: false,
            unordered_lists: false,
            columns: vec!["name"],
            rows: persons()
                .filter(|person| number(person.age).is_some_and(|value| value > 25.0))
                .map(|person| vec![name(person.name)])
                .collect(),
        },
        Question {
            text: "RETURN 1 AS i, 1.0 AS f, '1' AS s, null AS n, [1, 1.0, null] AS l",
            ordered: true,
            unordered_lists: false,
            columns: vec!["i", "f", "s", "n", "l"],
            rows: vec![vec![
                V::Int(1),
                V::Float(1.0),
                name("1"),
                V::Null,
                V::List(vec![V::Int(1), V::Float(1.0), V::Null]),
            ]],
        },
    ]
}

fn render(rows: &[Vec<V>], ordered: bool, unordered_lists: bool) -> Vec<String> {
    let mut rendered: Vec<String> = rows
        .iter()
        .map(|row| {
            let mut row = row.clone();
            if unordered_lists {
                row.iter_mut().for_each(V::sort_lists);
            }
            format!("{row:?}")
        })
        .collect();
    if !ordered {
        rendered.sort();
    }
    rendered
}

fn check_all(graph: &Graph, phase: &str) {
    for question in questions() {
        let result = graph
            .run(question.text, &[])
            .unwrap_or_else(|error| panic!("{phase} {}: {error}", question.text));
        assert_eq!(result.metadata().outcome, Outcome::Read);
        let (columns, rows) = tck::actual_table(&result);
        assert_eq!(columns, question.columns, "{phase} {}", question.text);
        assert_eq!(
            render(&rows, question.ordered, question.unordered_lists),
            render(&question.rows, question.ordered, question.unordered_lists),
            "{phase} {}",
            question.text
        );
    }
}

#[test]
fn ze56_tiny_graph_oracle_matches_before_and_after_reopen() {
    let mut graph = Graph::new("ze56-oracle");
    graph.setup(&setup_text());
    check_all(&graph, "fresh");
    graph.reopen();
    check_all(&graph, "reopened");
}

/// A write statement without RETURN commits its writes and returns no
/// columns and no rows; a later read sees what it wrote.
#[test]
fn ze56_write_without_return_commits_and_returns_no_rows() {
    let graph = Graph::new("ze56-no-return");
    let result = graph.run("CREATE (:A {v: 1}), (:A {v: 2})", &[]).unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));
    assert_eq!(result.metadata().rows, 0);
    assert!(result.pools().columns.is_empty());
    assert!(result.pools().nodes.is_empty());
    let read = graph
        .run("MATCH (a:A) RETURN a.v AS v ORDER BY v", &[])
        .unwrap();
    let (_, rows) = tck::actual_table(&read);
    assert_eq!(rows, vec![vec![V::Int(1)], vec![V::Int(2)]]);
}

/// A write without RETURN that fails at run time commits nothing: the same
/// statement with a zero divisor is refused as an expression error and
/// leaves the store as it was (its valid sibling node included), while the same text with a nonzero divisor
/// (the clean control) commits. (No MATCH: writing or reading a matched
/// Cypher-created entity under the writer is ZE-207.)
#[test]
fn ze56_no_return_write_failure_commits_nothing() {
    use zeppelin_embed::property_graph::query::QueryValue;
    use zeppelin_embed::property_graph::query::plan::ParameterBinding;
    let graph = Graph::new("ze56-no-return-fault");
    let text = "CREATE (:B {v: 7 / $d}), (:B {v: 1})";
    let zero = [ParameterBinding {
        name: "d",
        value: QueryValue::I64(0),
    }];
    let error = graph.run(text, &zero).map(|_| ()).unwrap_err();
    let StatementError::Query(error) = error else {
        panic!("expected a runtime refusal, got {error}")
    };
    assert_eq!(error.kind(), GraphQueryErrorKind::Expression, "{error}");
    assert!(error.nothing_committed());
    let count = "MATCH (b:B) RETURN count(b) AS n";
    assert_eq!(
        tck::actual_table(&graph.run(count, &[]).unwrap()).1,
        vec![vec![V::Int(0)]]
    );
    let one = [ParameterBinding {
        name: "d",
        value: QueryValue::I64(1),
    }];
    let result = graph.run(text, &one).unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));
    assert_eq!(result.metadata().rows, 0);
    assert_eq!(
        tck::actual_table(&graph.run(count, &[]).unwrap()).1,
        vec![vec![V::Int(2)]]
    );
}

/// A read whose rows exceed the statement's result capacity (1,024 rows by
/// default) is refused with no partial rows; one node fewer (1,024 rows)
/// is the clean control and returns every row.
#[test]
fn ze56_read_over_result_capacity_is_refused_without_rows() {
    for (nodes, fits) in [(33, false), (32, true)] {
        let graph = Graph::new("ze56-capacity");
        let create: Vec<String> = (0..nodes).map(|i| format!("({{i: {i}}})")).collect();
        graph.setup(&format!("CREATE {}", create.join(", ")));
        let outcome = graph.run("MATCH (a), (b) RETURN a.i AS a, b.i AS b", &[]);
        if fits {
            let result = outcome.unwrap();
            assert_eq!(result.metadata().rows, 1024);
        } else {
            let error = outcome.map(|_| ()).unwrap_err();
            let StatementError::Query(error) = error else {
                panic!("expected a capacity refusal, got {error}")
            };
            // ZE-208: capacity exhaustion is grouped as InvalidPlan today;
            // its fix must change this to GraphQueryErrorKind::Limit.
            assert_eq!(error.kind(), GraphQueryErrorKind::InvalidPlan, "{error}");
            assert!(error.nothing_committed());
        }
    }
}

/// A statement cancelled before it starts is a typed cancellation, and a
/// cancelled write commits nothing.
#[test]
fn ze56_cancelled_statements_are_typed_and_commit_nothing() {
    let graph = Graph::new("ze56-cancel");
    graph.setup("CREATE (:A {v: 1})");
    let token = CancelToken::new();
    token.cancel();
    let control = QueryControl::Cancel(token);
    for text in ["MATCH (a:A) RETURN a.v AS v", "MATCH (a:A) SET a.v = 2"] {
        let error = execute(
            graph.store(),
            &control,
            &GraphQueryOptions::default(),
            text,
            &[],
            CompileLimits::default(),
        )
        .map(|_| ())
        .unwrap_err();
        // The admission refuses a cancelled control before compiling.
        let StatementError::Query(error) = error else {
            panic!("{text}: expected the admission's typed refusal, got {error}")
        };
        assert_eq!(
            error.kind(),
            GraphQueryErrorKind::Cancelled,
            "{text}: {error}"
        );
        assert!(error.nothing_committed());
    }
    let read = graph.run("MATCH (a:A) RETURN a.v AS v", &[]).unwrap();
    assert_eq!(tck::actual_table(&read).1, vec![vec![V::Int(1)]]);
}
