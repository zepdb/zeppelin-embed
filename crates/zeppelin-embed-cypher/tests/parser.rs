#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use zeppelin_embed_cypher::parse;

#[test]
fn parses_literals_and_expression_precedence() {
    parse("RETURN -9223372036854775808, 9223372036854775807, .5, 1.2e-3, 'λ', null, true, false, 1+2*3").unwrap();
}

#[test]
fn parses_complete_clause_pattern_projection_and_write_inventory() {
    for query in [
        "MATCH (a:A:B {key: $key, x: 1+2})<-[r:R|S {p: true}]-(b), ()--() OPTIONAL MATCH (b)-[rels:T*0..3]->(c) WHERE c.x IS NOT NULL WITH DISTINCT a, collect(DISTINCT r) AS rs ORDER BY a.x DESC SKIP $skip LIMIT 2 WHERE a.x > 0 RETURN a, rs",
        "MATCH (a)-[*2]->(b), (b)-[*..3]-(c), (c)<-[*1..1]-(d) RETURN *",
        "CREATE (a:A:B {name: 'a'}), (b), (a)-[r:R {p: [1,2]}]->(b) SET a.x=1, r.y=$y, a:Added:Other REMOVE a.x, r.y, a:Other WITH a, 3 AS old DETACH DELETE a RETURN old LIMIT 0;",
        "MATCH (n) DELETE n",
        "CREATE () CREATE ()",
        "CALL ze.vector_search($q, 20, 'exact') YIELD node AS n, distance MATCH (n)-->(m) RETURN m, distance",
        "CALL ze.text_search('query', 10, []) YIELD node, score RETURN node, score",
        "CALL ze.hybrid_search([1.0], 'text', $k, 'auto', eligible) YIELD node, score, vector_distance, lexical_score RETURN node",
        "WITH 1 AS a RETURN a",
        "RETURN DISTINCT *, 1 AS x ORDER BY x ASC SKIP 0 LIMIT $limit",
    ] {
        parse(query).unwrap_or_else(|error| panic!("{query}: {error:?}"));
    }
}

use zeppelin_embed_cypher::{
    Ast, AstId, BinaryOp, Budget, CompileLimits, ErrorKind, LimitKind, NodeKind, ParseError,
    ResourceError, Resources, Span, UnaryOp, parse_bytes, parse_with,
};

fn expression(ast: &Ast, column: usize) -> AstId {
    let projection = ast.node(ast.root()).unwrap().children()[0];
    let item = ast.node(projection).unwrap().children()[column];
    ast.node(item).unwrap().children()[0]
}
fn binary(ast: &Ast, id: AstId, op: BinaryOp) -> (AstId, AstId) {
    let node = ast.node(id).unwrap();
    assert_eq!(node.kind, NodeKind::Binary(op));
    (node.children()[0], node.children()[1])
}
#[test]
fn precedence_and_chain_operands_are_preserved_in_the_arena() {
    let ast =
        parse("RETURN NOT 1+2*3<8<=9 AND true XOR false OR null, 8/2%3-1, 1 = 2 IN [2]").unwrap();
    let (xor, null) = binary(&ast, expression(&ast, 0), BinaryOp::Or);
    assert_eq!(ast.node(null).unwrap().kind, NodeKind::Null);
    let (and, _) = binary(&ast, xor, BinaryOp::Xor);
    let (not, _) = binary(&ast, and, BinaryOp::And);
    assert_eq!(ast.node(not).unwrap().kind, NodeKind::Unary(UnaryOp::Not));
    let (lt, le) = binary(&ast, ast.node(not).unwrap().children()[0], BinaryOp::And);
    let (add, middle) = binary(&ast, lt, BinaryOp::Lt);
    let (same_middle, _) = binary(&ast, le, BinaryOp::Le);
    assert_eq!(middle, same_middle);
    let (_, multiply) = binary(&ast, add, BinaryOp::Add);
    binary(&ast, multiply, BinaryOp::Multiply);
    let (remainder, _) = binary(&ast, expression(&ast, 1), BinaryOp::Subtract);
    let (divide, _) = binary(&ast, remainder, BinaryOp::Remainder);
    binary(&ast, divide, BinaryOp::Divide);
    let (_, membership) = binary(&ast, expression(&ast, 2), BinaryOp::Eq);
    binary(&ast, membership, BinaryOp::In);
}

#[test]
fn escapes_full_width_numbers_and_utf8_byte_spans_are_exact() {
    let query = "// λ\nRETURN `名``x`, '\\u03bb\\U0001F600\\uD83D\\uDE00\\b\\f\\n\\r\\t\\\\\\\"\\\'', -9223372036854775808, 9223372036854775807, 1.2e-3";
    let ast = parse(query).unwrap();
    let name = ast.node(expression(&ast, 0)).unwrap();
    let NodeKind::Variable(id) = name.kind else {
        panic!("name")
    };
    assert_eq!(ast.text(id), Some("名`x"));
    assert_eq!(&query[name.span.start..name.span.end], "`名``x`");
    let NodeKind::String(id) = ast.node(expression(&ast, 1)).unwrap().kind else {
        panic!("string")
    };
    assert_eq!(ast.text(id), Some("λ😀😀\u{8}\u{c}\n\r\t\\\"'"));
    assert_eq!(
        ast.node(expression(&ast, 2)).unwrap().kind,
        NodeKind::Integer(i64::MIN)
    );
    assert_eq!(
        ast.node(expression(&ast, 3)).unwrap().kind,
        NodeKind::Integer(i64::MAX)
    );
    assert_eq!(
        ast.node(expression(&ast, 4)).unwrap().kind,
        NodeKind::Float(0.0012)
    );
    let error = parse("RETURN 'λ', ☃").unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(error.span, Span { start: 13, end: 16 });
    assert_eq!(
        error
            .location("RETURN 'λ', ☃", &mut Budget::default())
            .unwrap(),
        (1, 13)
    );
    let error = parse("// λ\nRETURN '").unwrap_err();
    assert_eq!(
        error
            .location("// λ\nRETURN '", &mut Budget::default())
            .unwrap(),
        (2, 8)
    );
    for query in [
        "RETURN 9223372036854775808",
        "RETURN -9223372036854775809",
        "RETURN 1e309",
        "RETURN 1e",
        "RETURN 1e+2",
        "RETURN 0x10",
        "RETURN 012",
        "RETURN '\\x'",
        "RETURN '\\uD800'",
        "RETURN '\\uD800\\u0000'",
        "RETURN '\\uDC00'",
        "RETURN '\\U00110000'",
        "RETURN '\\uGGGG'",
        "RETURN `a\0b`",
        "RETURN 'unterminated",
        "RETURN 1 /* unterminated",
        "RETURN $1",
    ] {
        assert!(parse(query).is_err(), "{query}");
    }
}

#[test]
fn every_syntax_inventory_expression_has_a_preserved_node() {
    for query in [
        "RETURN null, true, false, 0, -1, +1, .25, 1e2, 1.5E-2, \"λ\", [], [1, [null], true, 'x'], $parameter, $`名`",
        "MATCH (n:A:B) RETURN n.p, n:A:B, [1,2][-1], labels(n), size('λ'), ze.node_id(n), ze.stored_text(n)",
        "MATCH ()-[r:R]->() RETURN type(r), ze.relationship_id(r), count(*), count(r), count(DISTINCT r), collect(r), collect(DISTINCT r)",
        "RETURN 1=1, 1<>2, 1<2, 1<=2, 2>1, 2>=1, 1 IN [1], null IS NULL, 1 IS NOT NULL, NOT true, true AND false, true OR false, true XOR false",
        "RETURN 1+2, 1-2, 1*2, 1/2, 1%2, 'abc' STARTS WITH 'a', 'abc' ENDS WITH 'c', 'abc' CONTAINS 'b'",
        "mAtCh (`MATCH`:`名`) rEtUrN `MATCH`, CoUnT(*)",
    ] {
        let ast = parse(query).unwrap_or_else(|e| panic!("{query}: {e}"));
        assert_eq!(ast.source(), query);
        let mut seen = 0;
        ast.visit(&mut Budget::default(), |_, node| {
            seen += 1;
            assert!(query.get(node.span.start..node.span.end).is_some());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, ast.nodes().len());
    }
}

#[test]
fn complete_input_rejects_unsupported_suffixes_and_profile_syntax() {
    let error = parse("CREATE (n) MERGE (m)").unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(error.span, Span { start: 11, end: 16 });
    for query in [
        "CREATE (n); RETURN n",
        "CREATE (n) garbage",
        "CREATE (n) RETURN n garbage",
        "CREATE (n) WITH n MATCH (m) RETURN m",
        "MATCH (n) SET n.x=1 WITH n OPTIONAL MATCH (m) RETURN m",
        "RETURN 1 UNION RETURN 2",
        "EXPLAIN RETURN 1",
        "PROFILE RETURN 1",
        "USE db RETURN 1",
        "START n=node(1) RETURN n",
        "LOAD CSV FROM 'x' AS x RETURN x",
        "BEGIN",
        "CREATE INDEX x",
        "MATCH p=(a)-->(b) RETURN p",
        "MATCH shortestPath((a)-->(b)) RETURN a",
        "MATCH (n:A|B) RETURN n",
        "MATCH (n $map) RETURN n",
        "MATCH (n {x:1, x:2}) RETURN n",
        "MATCH (n {x:1, `x`:2}) RETURN n",
        "MATCH ()-[*]->() RETURN 1",
        "MATCH ()-[*1..]->() RETURN 1",
        "MATCH ()-[*-1]->() RETURN 1",
        "MATCH ()-[*1.5]->() RETURN 1",
        "MATCH ()-[*$n]->() RETURN 1",
        "MATCH ()-[*3..1]->() RETURN 1",
        "MATCH ()-[*17]->() RETURN 1",
        "MATCH ()-[*4294967296]->() RETURN 1",
        "CREATE (a)-[:A*1]->(b)",
        "CREATE (a)-[:A]-(b)",
        "CREATE (a)-->(b)",
        "CREATE (a)-[:A|B]->(b)",
        "CREATE UNIQUE (n)",
        "MERGE (n)",
        "MATCH (n) SET n={x:1}",
        "MATCH (n) SET n += {}",
        "MATCH (n) SET n[$x]=1",
        "MATCH (n) REMOVE n[$x]",
        "MATCH (n) DELETE n.x",
        "MATCH (n) DELETE [n]",
        "CALL other.search() YIELD node RETURN node",
        "CALL ze.nope() YIELD node RETURN node",
        "CALL ze.text_search('a', 1) YIELD * RETURN 1",
        "CALL { RETURN 1 } RETURN 1",
        "CALL ze.text_search('a',1) YIELD node CREATE (n)",
        "CREATE (n) CALL ze.text_search('a',1) YIELD node RETURN n",
        "RETURN {x:1}",
        "RETURN n { .x }",
        "RETURN [x IN xs | x]",
        "RETURN [1,2][1..2]",
        "RETURN CASE WHEN true THEN 1 END",
        "RETURN coalesce(null,1)",
        "RETURN id(n)",
        "RETURN sum(n.x)",
        "RETURN 1^2",
        "RETURN 'x' =~ 'x'",
        "RETURN n LIMIT -1",
        "RETURN n SKIP n.x",
        "RETURN n LIMIT 1+1",
        "RETURN count(DISTINCT *)",
        "RETURN labels(DISTINCT n)",
        "RETURN collect(*)",
        "RETURN count()",
        "RETURN size(1,2)",
        "MATCH (n) WHERE true",
        "RETURN 1 WHERE true",
        "WITH 1 AS a",
        "CREATE (n) WITH n",
        "RETURN",
        "",
        "RETURN 1;;",
        "RETURN 1; RETURN 2",
        "RETURN α",
        "MATCH (a)–>(b) RETURN a",
        "RETURN $",
        "MATCH (n RETURN n",
        "MATCH (a)<-[:R]->(b) RETURN a",
        "RETURN 1 IS 2",
        "RETURN [1",
        "RETURN (1",
        "RETURN +",
        "RETURN '\\'",
    ] {
        assert!(parse(query).is_err(), "accepted {query}");
    }
}

#[test]
fn aggregate_placement_and_with_alias_are_profile_checked() {
    for query in [
        "RETURN count(*)+1",
        "RETURN collect(count(*))",
        "MATCH (n) WHERE count(n)>0 RETURN n",
        "CREATE (n {x:count(*)})",
        "MATCH (n) SET n.x=count(*)",
        "WITH 1+1 RETURN 1",
        "WITH n.x RETURN n.x",
    ] {
        assert!(parse(query).is_err(), "accepted {query}");
    }
    parse("MATCH (n) WITH n, count(*) AS c RETURN n, c").unwrap();
}

#[derive(Default)]
struct Schedule {
    polls: usize,
    reservations: usize,
    cancel_at: Option<usize>,
    fail_at: Option<usize>,
    bytes: usize,
}
impl Resources for Schedule {
    fn charge(&mut self, bytes: usize) -> Result<(), ResourceError> {
        self.reservations += 1;
        if self.fail_at == Some(self.reservations) {
            return Err(ResourceError::Memory);
        }
        self.bytes += bytes;
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ResourceError> {
        self.polls += 1;
        if self.cancel_at == Some(self.polls) {
            Err(ResourceError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[test]
fn limits_fire_on_actual_tokens_arena_depth_names_and_projection() {
    let cases = [
        (
            "RETURN 1",
            CompileLimits {
                text_bytes: 7,
                ..CompileLimits::default()
            },
            LimitKind::TextBytes,
        ),
        (
            "RETURN 1",
            CompileLimits {
                tokens: 1,
                ..CompileLimits::default()
            },
            LimitKind::Tokens,
        ),
        (
            "RETURN 1",
            CompileLimits {
                ast_nodes: 1,
                ..CompileLimits::default()
            },
            LimitKind::AstNodes,
        ),
        (
            "RETURN ((1))",
            CompileLimits {
                depth: 2,
                ..CompileLimits::default()
            },
            LimitKind::Depth,
        ),
        (
            "RETURN [[1]]",
            CompileLimits {
                list_depth: 1,
                ..CompileLimits::default()
            },
            LimitKind::ListDepth,
        ),
        (
            "RETURN $a, $b",
            CompileLimits {
                parameters: 1,
                ..CompileLimits::default()
            },
            LimitKind::Parameters,
        ),
        (
            "RETURN 1, 2",
            CompileLimits {
                columns: 1,
                ..CompileLimits::default()
            },
            LimitKind::Columns,
        ),
    ];
    for (text, limits, limit) in cases {
        assert_eq!(
            parse_with(text, limits, &mut Budget::default())
                .unwrap_err()
                .kind,
            ErrorKind::Limit(limit)
        );
        parse(text).unwrap();
    }
    parse_with(
        "RETURN $a, $a",
        CompileLimits {
            parameters: 1,
            columns: 2,
            ..CompileLimits::default()
        },
        &mut Budget::default(),
    )
    .unwrap();
    parse_with(
        "RETURN 1",
        CompileLimits {
            text_bytes: 8,
            tokens: 2,
            ast_nodes: 4,
            depth: 1,
            columns: 1,
            ..CompileLimits::default()
        },
        &mut Budget::default(),
    )
    .unwrap();
    assert_eq!(
        parse_with(
            "MATCH ()-[*2]->() RETURN 1",
            CompileLimits {
                path_hops: 1,
                ..CompileLimits::default()
            },
            &mut Budget::default()
        )
        .unwrap_err()
        .kind,
        ErrorKind::InvalidRange
    );
    for limits in [
        CompileLimits {
            text_bytes: 65537,
            ..CompileLimits::default()
        },
        CompileLimits {
            tokens: 8193,
            ..CompileLimits::default()
        },
        CompileLimits {
            ast_nodes: 4097,
            ..CompileLimits::default()
        },
        CompileLimits {
            depth: 65,
            ..CompileLimits::default()
        },
        CompileLimits {
            parameters: 257,
            ..CompileLimits::default()
        },
        CompileLimits {
            columns: 257,
            ..CompileLimits::default()
        },
        CompileLimits {
            list_depth: 17,
            ..CompileLimits::default()
        },
        CompileLimits {
            path_hops: 17,
            ..CompileLimits::default()
        },
    ] {
        assert_eq!(
            parse_with("RETURN 1", limits, &mut Budget::default())
                .unwrap_err()
                .kind,
            ErrorKind::InvalidLimits
        );
    }
    let deep = format!("RETURN {}1{}", "(".repeat(65), ")".repeat(65));
    assert_eq!(
        parse(&deep).unwrap_err().kind,
        ErrorKind::Limit(LimitKind::Depth)
    );
    let lists = format!("RETURN {}1{}", "[".repeat(17), "]".repeat(17));
    assert_eq!(
        parse(&lists).unwrap_err().kind,
        ErrorKind::Limit(LimitKind::ListDepth)
    );
}

#[test]
fn cancellation_and_allocation_faults_can_fire_at_every_reachable_site() {
    for text in [
        "// λ comment\n/* block */ MATCH (`名`:A:B {x: '\\u03bb\\U0001F600\\uD83D\\uDE00', y:$a})-[r:R|:S*0..3 {p:1}]->(m) WHERE n.x CONTAINS 'a' WITH n, count(*) AS c ORDER BY c DESC SKIP $s LIMIT 20 WHERE c>1 RETURN n, c",
        "CREATE (n {x:[1,2]}) SET n.x=n.x+1, n:A:B REMOVE n:A, n.x DETACH DELETE n RETURN 1",
        "CALL ze.vector_search([1.0,2.0],10,'exact') YIELD node AS n, distance RETURN n, distance",
    ] {
        let mut baseline = Schedule::default();
        let clean = parse_with(text, CompileLimits::default(), &mut baseline).unwrap();
        assert!(baseline.polls > text.chars().count());
        assert!(baseline.reservations > 10);
        for point in 1..=baseline.polls {
            let mut resources = Schedule {
                cancel_at: Some(point),
                ..Schedule::default()
            };
            assert_eq!(
                parse_with(text, CompileLimits::default(), &mut resources)
                    .unwrap_err()
                    .kind,
                ErrorKind::Resource(ResourceError::Cancelled),
                "checkpoint {point}"
            );
            assert_eq!(resources.polls, point);
        }
        for point in 1..=baseline.reservations {
            let mut resources = Schedule {
                fail_at: Some(point),
                ..Schedule::default()
            };
            assert_eq!(
                parse_with(text, CompileLimits::default(), &mut resources)
                    .unwrap_err()
                    .kind,
                ErrorKind::Resource(ResourceError::Memory),
                "reservation {point}"
            );
            assert_eq!(resources.reservations, point);
        }
        let restored =
            parse_with(text, CompileLimits::default(), &mut Schedule::default()).unwrap();
        assert_eq!(clean.nodes(), restored.nodes());
        eprintln!(
            "frontend faults: checkpoints={} reservations={} charged_bytes={}",
            baseline.polls, baseline.reservations, baseline.bytes
        );
    }
    let ast = parse("RETURN 1+2*3").unwrap();
    for point in 1..=ast.nodes().len() {
        let mut resources = Schedule {
            cancel_at: Some(point),
            ..Schedule::default()
        };
        assert_eq!(
            ast.visit(&mut resources, |_, _| Ok(())).unwrap_err().kind,
            ErrorKind::Resource(ResourceError::Cancelled)
        );
    }
    let flag = std::sync::atomic::AtomicBool::new(true);
    assert_eq!(
        parse_with(
            "RETURN 1",
            CompileLimits::default(),
            &mut Budget::new(usize::MAX, Some(&flag))
        )
        .unwrap_err()
        .kind,
        ErrorKind::Resource(ResourceError::Cancelled)
    );
    flag.store(false, std::sync::atomic::Ordering::Relaxed);
    let mut budget = Budget::new(usize::MAX, Some(&flag));
    parse_with("RETURN 1", CompileLimits::default(), &mut budget).unwrap();
    assert!(budget.charged_bytes() > 0);
    assert_eq!(
        parse_with(
            "RETURN 1",
            CompileLimits::default(),
            &mut Budget::new(0, None)
        )
        .unwrap_err()
        .kind,
        ErrorKind::Resource(ResourceError::Memory)
    );
}

#[test]
fn malformed_bytes_diagnostics_and_iterative_visitor_fail_explicitly() {
    let error = parse_bytes(
        b"RETURN '\xff'",
        CompileLimits::default(),
        &mut Budget::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Syntax);
    assert_eq!(error.span, Span { start: 8, end: 9 });
    assert!(
        parse_bytes(
            b"RETURN '\xe2\x82",
            CompileLimits::default(),
            &mut Budget::default()
        )
        .is_err()
    );
    assert_eq!(
        parse_bytes(
            b"RETURN 1",
            CompileLimits {
                text_bytes: 1,
                ..CompileLimits::default()
            },
            &mut Budget::default()
        )
        .unwrap_err()
        .kind,
        ErrorKind::Limit(LimitKind::TextBytes)
    );
    parse_bytes(
        b"RETURN 1",
        CompileLimits::default(),
        &mut Budget::default(),
    )
    .unwrap();
    let bad_span = ParseError {
        kind: ErrorKind::Syntax,
        span: Span { start: 1, end: 2 },
        message: "test",
    };
    assert!(bad_span.location("λ", &mut Budget::default()).is_err());
    assert_eq!(bad_span.to_string(), "test at bytes 1..2");
    assert!(
        bad_span
            .location(
                "ab",
                &mut Schedule {
                    cancel_at: Some(1),
                    ..Schedule::default()
                }
            )
            .is_err()
    );
    assert_eq!(
        parse("RETURN 1")
            .unwrap()
            .visit(&mut Budget::default(), |_, _| Err(bad_span))
            .unwrap_err(),
        bad_span
    );
}

#[test]
fn selected_original_tck_statements_parse_without_claiming_execution_conformance() {
    let fixture = include_str!("fixtures/selected-tck-syntax.txt");
    let mut passed = 0;
    let mut rejections = 0;
    for part in fixture.split("\n---\n").skip(1) {
        let (label, query) = part.split_once('\n').unwrap();
        let result = parse(query);
        if label.starts_with("reject ") {
            assert!(result.is_err(), "{label}: accepted {query}");
            rejections += 1;
        } else {
            result.unwrap_or_else(|error| panic!("{label}: {error:?}\n{query}"));
            passed += 1;
        }
    }
    assert_eq!((passed, rejections), (153, 2));
}

#[test]
fn reserved_words_require_escaping_as_variables_but_allow_schema_names() {
    for query in [
        "MATCH (RETURN) RETURN 1",
        "CREATE (null)",
        "RETURN 1 AS MATCH",
        "WITH 1 AS true RETURN 1",
        "MATCH ()-[RETURN:R]->() RETURN 1",
    ] {
        assert!(parse(query).is_err(), "accepted {query}");
    }
    parse("MATCH (`RETURN`:RETURN {MATCH:1}) RETURN `RETURN`.MATCH AS `WITH`").unwrap();
}

#[test]
fn truncated_utf8_error_span_covers_the_remaining_bytes() {
    let error = parse_bytes(
        b"RETURN '\xe2\x82",
        CompileLimits::default(),
        &mut Budget::default(),
    )
    .unwrap_err();
    assert_eq!(error.span, Span { start: 8, end: 10 });
}

#[test]
fn grouping_preserves_top_level_aggregates_and_variable_forwarding() {
    for query in [
        "RETURN (count(*))",
        "MATCH (n) RETURN ((collect(n)))",
        "MATCH (n) WITH (n) RETURN n",
        "MATCH (n) WITH ((n)), (count(*)) AS c RETURN n,c",
    ] {
        parse(query).unwrap_or_else(|error| panic!("{query}: {error:?}"));
    }
    for query in [
        "RETURN (count(*))+1",
        "RETURN (collect(count(*)))",
        "WITH (1+1) RETURN 1",
    ] {
        assert!(parse(query).is_err());
    }
}

#[test]
fn duplicate_property_error_names_the_second_key() {
    let error = parse("MATCH (n {x:1, x:2}) RETURN n").unwrap_err();
    assert_eq!(error.kind, ErrorKind::DuplicateProperty);
    assert_eq!(error.span, Span { start: 15, end: 16 });
}

#[test]
fn byte_admission_validates_limits_before_scanning_input() {
    let error = parse_bytes(
        b"\xff",
        CompileLimits {
            text_bytes: usize::MAX,
            ..CompileLimits::default()
        },
        &mut Budget::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidLimits);
}

#[test]
fn patterns_calls_and_projection_modifiers_preserve_lowering_payload() {
    use zeppelin_embed_cypher::{Direction, PathBounds, Procedure};
    let ast = parse("MATCH (a:A:B {p:$p})<-[r:R|S*0..3 {x:1}]-(b) RETURN a").unwrap();
    let clauses = ast.node(ast.root()).unwrap().children();
    assert_eq!(clauses.len(), 2);
    let matching = ast.node(clauses[0]).unwrap();
    assert_eq!(matching.kind, NodeKind::Match { optional: false });
    let pattern = ast.node(matching.children()[0]).unwrap();
    assert_eq!(pattern.kind, NodeKind::Pattern);
    assert_eq!(pattern.children().len(), 3);
    let node = ast.node(pattern.children()[0]).unwrap();
    let NodeKind::NodePattern {
        variable: Some(name),
    } = node.kind
    else {
        panic!("node")
    };
    assert_eq!(ast.text(name), Some("a"));
    let labels: Vec<_> = node
        .children()
        .iter()
        .filter_map(|id| match ast.node(*id).unwrap().kind {
            NodeKind::Name(name) => ast.text(name),
            _ => None,
        })
        .collect();
    assert_eq!(labels, ["A", "B"]);
    let rel = ast.node(pattern.children()[1]).unwrap();
    let NodeKind::RelationshipPattern {
        variable: Some(name),
        direction,
        bounds,
    } = rel.kind
    else {
        panic!("rel")
    };
    assert_eq!(ast.text(name), Some("r"));
    assert_eq!(direction, Direction::Incoming);
    assert_eq!(bounds, Some(PathBounds { lower: 0, upper: 3 }));
    let types: Vec<_> = rel
        .children()
        .iter()
        .filter_map(|id| match ast.node(*id).unwrap().kind {
            NodeKind::Name(name) => ast.text(name),
            _ => None,
        })
        .collect();
    assert_eq!(types, ["R", "S"]);
    let props = ast.node(rel.children()[2]).unwrap();
    assert_eq!(props.kind, NodeKind::Properties);
    let property = ast.node(props.children()[0]).unwrap();
    let NodeKind::Property(name) = property.kind else {
        panic!("property")
    };
    assert_eq!(ast.text(name), Some("x"));
    assert_eq!(
        ast.node(property.children()[0]).unwrap().kind,
        NodeKind::Integer(1)
    );

    let ast = parse("RETURN DISTINCT 1 AS one ORDER BY one DESC SKIP $s LIMIT 2").unwrap();
    let projection = ast
        .node(ast.node(ast.root()).unwrap().children()[0])
        .unwrap();
    assert_eq!(
        projection.kind,
        NodeKind::Projection {
            with: false,
            distinct: true
        }
    );
    assert_eq!(projection.children().len(), 4);
    let item = ast.node(projection.children()[0]).unwrap();
    let NodeKind::ProjectionItem { alias: Some(alias) } = item.kind else {
        panic!("alias")
    };
    assert_eq!(ast.text(alias), Some("one"));
    assert_eq!(
        ast.node(projection.children()[1]).unwrap().kind,
        NodeKind::Order { descending: true }
    );
    assert_eq!(
        ast.node(projection.children()[2]).unwrap().kind,
        NodeKind::Skip
    );
    assert_eq!(
        ast.node(projection.children()[3]).unwrap().kind,
        NodeKind::Limit
    );

    for (name, procedure) in [
        ("vector_search", Procedure::VectorSearch),
        ("text_search", Procedure::TextSearch),
        ("hybrid_search", Procedure::HybridSearch),
    ] {
        let ast = parse(&format!(
            "CALL ze.{name}($q,20,'exact') YIELD node AS n RETURN n"
        ))
        .unwrap();
        let call = ast
            .node(ast.node(ast.root()).unwrap().children()[0])
            .unwrap();
        assert_eq!(call.kind, NodeKind::Call(procedure));
        assert_eq!(call.children().len(), 4);
        assert_eq!(
            ast.node(call.children()[1]).unwrap().kind,
            NodeKind::Integer(20)
        );
        let NodeKind::Yield {
            name,
            alias: Some(alias),
        } = ast.node(call.children()[3]).unwrap().kind
        else {
            panic!("yield")
        };
        assert_eq!((ast.text(name), ast.text(alias)), (Some("node"), Some("n")));
    }
}

#[test]
fn unknown_function_and_procedure_errors_identify_the_name() {
    for (query, start, end) in [
        ("RETURN id(n)", 7, 9),
        ("RETURN ze.nope(n)", 10, 14),
        ("CALL ze.nope() YIELD n RETURN n", 8, 12),
        ("CALL other.search() YIELD n RETURN n", 5, 10),
    ] {
        let error = parse(query).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(error.span, Span { start, end });
    }
}

#[test]
fn recognized_out_of_profile_syntax_has_a_distinct_profile_error() {
    for query in [
        "MATCH (n) UNWIND [1] AS x RETURN x",
        "CALL { RETURN 1 } RETURN 1",
        "MATCH (n) DELETE [n]",
        "RETURN {x:1}",
        "RETURN [1,2][1..2]",
        "RETURN [1,2][..1]",
        "RETURN [x IN xs | x]",
        "MATCH (n:$label) RETURN n",
        "MATCH ()-[:$type]->() RETURN 1",
        "UNWIND [1] AS x RETURN x",
        "MERGE (n)",
        "EXPLAIN RETURN 1",
        "CALL ze.text_search('x',1) YIELD * RETURN 1",
    ] {
        let error = parse(query).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported, "{query}: {error:?}");
    }
}
