//! Original-TCK fixture reader and an independent value model. Expected
//! cells are parsed from the TCK's own text; actual cells are read from the
//! completed result's typed pools. Neither side is rendered by the other.
#![allow(
    dead_code,
    reason = "shared by several test crates, each using a subset"
)]
use std::collections::BTreeMap;
use zeppelin_embed::property_graph::query::completed::{
    CompletedGraphResult, ListKind, Pools, Value,
};

/// One comparable value. Integers and floats stay distinct types; nodes and
/// relationships compare by labels/type and properties, as the TCK does.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub(crate) enum V {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<V>),
    Node(Vec<String>, BTreeMap<String, V>),
    Rel(String, BTreeMap<String, V>),
}

impl V {
    /// Sorts every list's elements, for "ignoring element order for lists".
    pub(crate) fn sort_lists(&mut self) {
        match self {
            V::List(items) => {
                for item in items.iter_mut() {
                    item.sort_lists();
                }
                items.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            }
            V::Node(_, props) | V::Rel(_, props) => props.values_mut().for_each(V::sort_lists),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Expected side: the TCK cell grammar used by the selected scenarios.
// ---------------------------------------------------------------------------

pub(crate) fn parse_value(text: &str) -> V {
    let mut parser = Parser {
        chars: text.trim().chars().collect(),
        at: 0,
    };
    let value = parser.value();
    parser.skip();
    assert_eq!(
        parser.at,
        parser.chars.len(),
        "trailing TCK text in {text:?}"
    );
    value
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }
    fn skip(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += 1;
        }
    }
    fn eat(&mut self, c: char) -> bool {
        self.skip();
        if self.peek() == Some(c) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: char) {
        assert!(
            self.eat(c),
            "expected {c:?} at {} in {:?}",
            self.at,
            self.text()
        );
    }
    fn text(&self) -> String {
        self.chars.iter().collect()
    }
    fn word(&mut self) -> String {
        self.skip();
        let start = self.at;
        while self
            .peek()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '-' || c == '+')
        {
            self.at += 1;
        }
        self.chars[start..self.at].iter().collect()
    }
    fn string(&mut self) -> String {
        self.expect('\'');
        let start = self.at;
        while self.peek() != Some('\'') {
            assert!(self.peek().is_some(), "unterminated string");
            self.at += 1;
        }
        let text = self.chars[start..self.at].iter().collect();
        self.at += 1;
        text
    }
    fn properties(&mut self) -> BTreeMap<String, V> {
        let mut map = BTreeMap::new();
        if self.eat('{') && !self.eat('}') {
            loop {
                let key = self.word();
                self.expect(':');
                let value = self.value();
                assert!(map.insert(key, value).is_none(), "duplicate TCK key");
                if self.eat('}') {
                    break;
                }
                self.expect(',');
            }
        }
        map
    }
    fn value(&mut self) -> V {
        self.skip();
        match self.peek() {
            Some('\'') => V::Str(self.string()),
            Some('(') => {
                self.at += 1;
                let mut labels = Vec::new();
                while self.eat(':') {
                    labels.push(self.word());
                }
                labels.sort();
                let props = self.properties();
                self.expect(')');
                V::Node(labels, props)
            }
            Some('[') => {
                self.at += 1;
                if self.eat(':') {
                    let rel_type = self.word();
                    let props = self.properties();
                    self.expect(']');
                    return V::Rel(rel_type, props);
                }
                let mut items = Vec::new();
                if !self.eat(']') {
                    loop {
                        items.push(self.value());
                        if self.eat(']') {
                            break;
                        }
                        self.expect(',');
                    }
                }
                V::List(items)
            }
            _ => {
                let word = self.word();
                match word.as_str() {
                    "null" => V::Null,
                    "true" => V::Bool(true),
                    "false" => V::Bool(false),
                    _ if word.contains('.') => V::Float(word.parse().expect("TCK float")),
                    _ => V::Int(
                        word.parse()
                            .unwrap_or_else(|_| panic!("TCK value {word:?}")),
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Actual side: the completed result's typed pools.
// ---------------------------------------------------------------------------

fn text(
    result: &CompletedGraphResult,
    span: zeppelin_embed::property_graph::query::completed::Span,
) -> String {
    result.string(span).expect("result string span").to_owned()
}

fn properties(
    result: &CompletedGraphResult,
    pools: Pools<'_>,
    span: zeppelin_embed::property_graph::query::completed::Span,
) -> BTreeMap<String, V> {
    let start = span.start as usize;
    pools.properties[start..start + span.len as usize]
        .iter()
        .map(|property| {
            (
                text(result, property.name),
                actual_value(result, pools, pools.values[property.value.0 as usize]),
            )
        })
        .collect()
}

pub(crate) fn actual_value(result: &CompletedGraphResult, pools: Pools<'_>, value: Value) -> V {
    match value {
        Value::Null => V::Null,
        Value::Bool(value) => V::Bool(value),
        Value::I64(value) => V::Int(value),
        Value::F64(bits) => V::Float(f64::from_bits(bits)),
        Value::String(span) => V::Str(text(result, span)),
        Value::List { children, element } => {
            if children.len == 0 {
                return V::List(vec![]);
            }
            assert_ne!(
                element,
                ListKind::Empty,
                "query lists are never canonical empties"
            );
            let start = children.start as usize;
            V::List(
                pools.children[start..start + children.len as usize]
                    .iter()
                    .map(|child| actual_value(result, pools, pools.values[child.0 as usize]))
                    .collect(),
            )
        }
        Value::Node(index) => {
            let node = pools.nodes[index as usize];
            let start = node.labels.start as usize;
            let mut labels: Vec<String> = pools.names[start..start + node.labels.len as usize]
                .iter()
                .map(|span| text(result, *span))
                .collect();
            labels.sort();
            V::Node(labels, properties(result, pools, node.properties))
        }
        Value::Relationship(index) => {
            let relationship = pools.relationships[index as usize];
            V::Rel(
                text(result, relationship.relationship_type),
                properties(result, pools, relationship.properties),
            )
        }
    }
}

/// The result's column names and rows, in result order.
pub(crate) fn actual_table(result: &CompletedGraphResult) -> (Vec<String>, Vec<Vec<V>>) {
    let pools = result.pools();
    let columns: Vec<String> = pools.columns.iter().map(|c| text(result, c.name)).collect();
    let rows = (0..result.metadata().rows as usize)
        .map(|row| {
            (0..columns.len())
                .map(|column| actual_value(result, pools, *result.cell(row, column).expect("cell")))
                .collect()
        })
        .collect();
    (columns, rows)
}

// ---------------------------------------------------------------------------
// Fixture reader
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum Expect {
    /// `bag`, `ordered` or `bag-lists-unordered`, with header and rows.
    Table {
        mode: String,
        header: Vec<String>,
        rows: Vec<Vec<V>>,
    },
    Empty,
    RuntimeError {
        category: String,
        detail: String,
    },
    CompileError(String),
    RejectProfile,
    /// An in-profile example row of a profile-rejected outline, with the
    /// example's own `result`; a local observation, never an original pass.
    LocalExample(Option<V>),
}

#[derive(Debug)]
pub(crate) struct Scenario {
    pub(crate) coordinate: String,
    pub(crate) setup: Vec<String>,
    pub(crate) parameters: Vec<(String, V)>,
    pub(crate) query: String,
    pub(crate) expect: Expect,
    pub(crate) side_effects: BTreeMap<String, u64>,
}

fn cells(line: &str) -> Vec<String> {
    let line = line.trim();
    line[1..line.len() - 1]
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

pub(crate) fn scenarios(fixture: &str) -> Vec<Scenario> {
    let mut out = Vec::new();
    for block in fixture.split("\n=== ").skip(1) {
        let mut lines = block.lines();
        let coordinate = lines.next().expect("coordinate").to_owned();
        let (mut setup, mut parameters, mut query) = (Vec::new(), Vec::new(), String::new());
        let mut expect = None;
        let mut side_effects = BTreeMap::new();
        let mut section = "";
        let mut table: Vec<String> = Vec::new();
        for line in lines {
            if let Some(body) = line.strip_prefix("  ") {
                match section {
                    "setup" => {
                        let last: &mut String = setup.last_mut().expect("setup statement");
                        last.push_str(body);
                        last.push('\n');
                    }
                    "query" => {
                        query.push_str(body);
                        query.push('\n');
                    }
                    "table" => table.push(body.to_owned()),
                    "side-effects" => {
                        let cells = cells(body);
                        assert_eq!(cells.len(), 2, "{coordinate}: side-effect row");
                        assert!(
                            side_effects
                                .insert(
                                    cells[0].clone(),
                                    cells[1].parse().expect("side-effect count")
                                )
                                .is_none(),
                            "{coordinate}: duplicate effect"
                        );
                    }
                    other => panic!("indented line in {other:?} of {coordinate}"),
                }
                continue;
            }
            if line == "setup" {
                setup.push(String::new());
                section = "setup";
            } else if line == "query" {
                section = "query";
            } else if let Some(parameter) = line.strip_prefix("parameter ") {
                let (name, value) = parameter.split_once(" = ").expect("parameter");
                parameters.push((name.to_owned(), parse_value(value)));
            } else if let Some(kind) = line.strip_prefix("expect compile-error ") {
                expect = Some(Expect::CompileError(kind.to_owned()));
            } else if line == "expect reject-profile" {
                expect = Some(Expect::RejectProfile);
            } else if line == "expect local-example" {
                expect = Some(Expect::LocalExample(None));
            } else if let Some(value) = line.strip_prefix("result ") {
                expect = Some(Expect::LocalExample(Some(parse_value(value))));
            } else if line == "expect empty" {
                expect = Some(Expect::Empty);
                section = "";
            } else if let Some(error) = line.strip_prefix("expect runtime-error ") {
                let (category, detail) =
                    error.split_once(' ').expect("runtime category and detail");
                expect = Some(Expect::RuntimeError {
                    category: category.to_owned(),
                    detail: detail.to_owned(),
                });
                section = "";
            } else if line == "side-effects" {
                section = "side-effects";
            } else if let Some(mode) = line.strip_prefix("expect ") {
                expect = Some(Expect::Table {
                    mode: mode.to_owned(),
                    header: Vec::new(),
                    rows: Vec::new(),
                });
                section = "table";
            } else if line == "side-effects none" {
                section = "";
            } else {
                panic!("unknown fixture line {line:?} in {coordinate}");
            }
        }
        let mut expect = expect.expect("expectation");
        if let Expect::Table { header, rows, .. } = &mut expect {
            let mut table = table.iter();
            *header = cells(table.next().expect("header"));
            *rows = table
                .map(|row| cells(row).iter().map(|cell| parse_value(cell)).collect())
                .collect();
        }
        out.push(Scenario {
            coordinate,
            setup,
            parameters,
            query,
            expect,
            side_effects,
        });
    }
    out
}
