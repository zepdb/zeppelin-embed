use super::*;
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Cell {
    Null,
    Bool(bool),
    I64(i64),
    String(String),
    Node(u128),
    Relationship(u128),
    List(Vec<Cell>),
    Score(u64),
}
pub type Row = Vec<Cell>;
#[derive(Clone, Debug)]
pub enum Query {
    ProjectEvidence {
        project: u128,
        limit: usize,
    },
    SemanticContext {
        vector: Vec<f32>,
        k: usize,
    },
    AliceProjectRanking {
        person: u128,
        project: u128,
        vector: Vec<f32>,
        k: usize,
    },
    LexicalEvidence {
        terms: Vec<String>,
        phrase: bool,
        k: usize,
    },
    HybridProjectEvidence {
        project: u128,
        vector: Vec<f32>,
        terms: Vec<String>,
        phrase: bool,
        k: usize,
        alpha: f64,
    },
    BoundedEvidence {
        meeting: u128,
        min: u8,
        max: u8,
    },
}
/// Computes complete primitive logical expectations, independent of any engine.
pub fn query(graph: &Snapshot, query: &Query) -> Result<Vec<Row>, String> {
    let nodes = graph
        .nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    if nodes.len() != graph.nodes.len() {
        return Err("duplicate primitive node ID".into());
    }
    let edges = graph
        .relationships
        .iter()
        .filter(|edge| nodes.contains_key(&edge.source) && nodes.contains_key(&edge.target))
        .collect::<Vec<_>>();
    match query {
        Query::ProjectEvidence { project, limit } => {
            let mut rows = Vec::new();
            for about in edges
                .iter()
                .filter(|e| e.relationship_type == "ABOUT" && e.target == *project)
            {
                for support in edges
                    .iter()
                    .filter(|e| e.relationship_type == "SUPPORTED_BY" && e.source == about.source)
                {
                    for source in edges.iter().filter(|e| {
                        e.relationship_type == "HAS_CHUNK" && e.target == support.target
                    }) {
                        let item = nodes[&about.source];
                        let meeting = nodes[&source.source];
                        let chunk = nodes[&source.target];
                        let timestamp = match meeting.properties.get("timestamp") {
                            Some(Property::Scalar(Scalar::I64(v))) => *v,
                            _ => return Err("meeting missing timestamp".into()),
                        };
                        rows.push((
                            (std::cmp::Reverse(timestamp), item.id, meeting.id, chunk.id),
                            vec![
                                Cell::Node(item.id),
                                property(item, "name"),
                                Cell::Node(meeting.id),
                                property(meeting, "name"),
                                Cell::Node(chunk.id),
                                property(chunk, "excerpt"),
                            ],
                        ));
                    }
                }
            }
            rows.sort_by_key(|(key, _)| *key);
            rows.truncate(*limit);
            Ok(rows.into_iter().map(|(_, row)| row).collect())
        }
        Query::SemanticContext { vector, k } => {
            valid_vector(vector)?;
            // Rank the full live original-vector population before expansion.
            // An unexpandable seed still occupies its global top-k position.
            let mut seeds = Vec::new();
            for node in &graph.nodes {
                if let Some(bits) = &node.vector {
                    seeds.push((node.id, distance(bits, vector)?));
                }
            }
            seeds.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            seeds.truncate(*k);
            let mut rows = Vec::new();
            for (seed, distance) in &seeds {
                let chunk = nodes[seed];
                let mut local = Vec::new();
                for source in edges
                    .iter()
                    .filter(|e| e.relationship_type == "HAS_CHUNK" && e.target == *seed)
                {
                    for mention in edges
                        .iter()
                        .filter(|e| e.relationship_type == "MENTIONS" && e.source == *seed)
                    {
                        let meeting = nodes[&source.source];
                        let entity = nodes[&mention.target];
                        local.push((
                            (meeting.id, entity.id, source.id, mention.id),
                            vec![
                                Cell::Node(*seed),
                                score(*distance),
                                property(chunk, "excerpt"),
                                Cell::Node(meeting.id),
                                property(meeting, "name"),
                                Cell::Node(entity.id),
                                property(entity, "name"),
                            ],
                        ));
                    }
                }
                local.sort_by_key(|(key, _)| *key);
                rows.extend(local.into_iter().map(|(_, row)| row));
            }
            Ok(rows)
        }
        Query::AliceProjectRanking {
            person,
            project,
            vector,
            k,
        } => {
            valid_vector(vector)?;
            let meetings = project_meetings(&edges, *project)
                .intersection(
                    &edges
                        .iter()
                        .filter(|e| e.source == *person && e.relationship_type == "PARTICIPATED_IN")
                        .map(|e| e.target)
                        .collect(),
                )
                .copied()
                .collect::<BTreeSet<_>>();
            let eligible = edges
                .iter()
                .filter(|e| e.relationship_type == "HAS_CHUNK" && meetings.contains(&e.source))
                .map(|e| e.target)
                .collect::<BTreeSet<_>>();
            let mut seeds = Vec::new();
            for id in eligible {
                if let Some(bits) = &nodes[&id].vector {
                    seeds.push((distance(bits, vector)?, id));
                }
            }
            seeds.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            seeds.truncate(*k);
            let mut rows = Vec::new();
            for (d, id) in seeds {
                let mut sources = edges
                    .iter()
                    .filter(|e| e.relationship_type == "HAS_CHUNK" && e.target == id)
                    .collect::<Vec<_>>();
                sources.sort_by_key(|e| (e.source, e.id));
                for e in sources {
                    rows.push(vec![
                        Cell::Node(id),
                        score(d),
                        property(nodes[&id], "excerpt"),
                        Cell::Node(e.source),
                    ]);
                }
            }
            Ok(rows)
        }
        Query::LexicalEvidence { terms, phrase, k } => {
            let lexical = lexical(graph, terms, *phrase)?;
            let mut ranked = lexical
                .scores
                .iter()
                .filter(|(_, s)| **s > 0.0)
                .map(|(id, s)| (*s, *id))
                .collect::<Vec<_>>();
            ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            ranked.truncate(*k);
            Ok(ranked
                .into_iter()
                .map(|(s, id)| {
                    let n = nodes[&id];
                    vec![
                        Cell::Node(id),
                        property(n, "name"),
                        Cell::Bool(n.text.is_some()),
                        text(n),
                        score(s),
                    ]
                })
                .collect())
        }
        Query::HybridProjectEvidence {
            project,
            vector,
            terms,
            phrase,
            k,
            alpha,
        } => {
            valid_vector(vector)?;
            if !alpha.is_finite() || !(0.0..=1.0).contains(alpha) {
                return Err("invalid query alpha".into());
            }
            let lexical = lexical(graph, terms, *phrase)?;
            let maximum = lexical.scores.values().copied().fold(0.0_f64, f64::max);
            let meetings = project_meetings(&edges, *project);
            let mut eligible = meetings.clone();
            eligible.extend(
                edges
                    .iter()
                    .filter(|e| {
                        meetings.contains(&e.source)
                            && matches!(e.relationship_type.as_str(), "HAS_CHUNK" | "HAS_ITEM")
                    })
                    .map(|e| e.target),
            );
            let mut distances = BTreeMap::new();
            // Store policy v1 over the full live original-f32 population.
            // Derive this enclosure independently; do not import product helpers
            // or compressed factors into the expected-value model.
            let scaled = vector.len() as f64 * f64::EPSILON;
            if scaled >= 1.0 {
                return Err("unbounded reference enclosure".into());
            }
            let error = 2.0 * scaled / (1.0 - scaled);
            let mut max_norm = 0.0_f64;
            for n in &graph.nodes {
                if let Some(bits) = &n.vector {
                    let d = distance(bits, vector)?;
                    let mut norm = 0.0;
                    for b in bits {
                        let f = f64::from(f32::from_bits(*b));
                        norm += f * f;
                    }
                    max_norm = max_norm.max((norm / (1.0 - error)).sqrt().next_up());
                    if eligible.contains(&n.id) {
                        distances.insert(n.id, d);
                    }
                }
            }
            let query_norm = vector
                .iter()
                .fold(0.0_f64, |sum, v| sum + f64::from(*v) * f64::from(*v))
                / (1.0 - error);
            let query_max = query_norm.sqrt().next_up();
            let sum = query_max + max_norm;
            let ceiling = (sum * sum * (1.0 + error)).next_up();
            let vector_nonempty = !distances.is_empty();
            let text_nonempty = eligible
                .iter()
                .any(|id| lexical.scores.get(id).is_some_and(|s| *s > 0.0));
            let weight = if !vector_nonempty {
                0.0
            } else if !text_nonempty {
                1.0
            } else {
                *alpha
            };
            let mut rows = Vec::new();
            for id in eligible {
                let d = distances.get(&id).copied();
                let b = lexical.scores.get(&id).copied();
                if d.is_none() && b.is_none_or(|s| s == 0.0) {
                    continue;
                }
                let v = d.map_or(0.0, |d| {
                    if ceiling == 0.0 {
                        1.0
                    } else {
                        (1.0 - d / ceiling).clamp(0.0, 1.0)
                    }
                });
                let l = if maximum == 0.0 {
                    0.0
                } else {
                    b.unwrap_or(0.0) / maximum
                };
                let fused = weight * v + (1.0 - weight) * l;
                let n = nodes[&id];
                rows.push((
                    fused,
                    id,
                    vec![
                        Cell::Node(id),
                        property(n, "name"),
                        score(fused),
                        d.map_or(Cell::Null, score),
                        b.map_or(Cell::Null, score),
                        Cell::Bool(d.is_some()),
                        Cell::Bool(b.is_some()),
                        text(n),
                    ],
                ));
            }
            rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            rows.truncate(*k);
            Ok(rows.into_iter().map(|(_, _, r)| r).collect())
        }
        Query::BoundedEvidence { meeting, min, max } => {
            if *min > *max || *max > 16 {
                return Err("invalid path bounds".into());
            }
            if !nodes.contains_key(meeting) {
                return Ok(Vec::new());
            }
            let mut pending = vec![(*meeting, Vec::<u128>::new())];
            let mut rows = Vec::new();
            while let Some((node, path)) = pending.pop() {
                if path.len() >= usize::from(*min) {
                    rows.push(vec![
                        Cell::Node(node),
                        Cell::List(path.iter().copied().map(Cell::Relationship).collect()),
                    ]);
                }
                if path.len() == usize::from(*max) {
                    continue;
                }
                for edge in edges.iter().filter(|e| {
                    e.source == node
                        && matches!(e.relationship_type.as_str(), "HAS_CHUNK" | "MENTIONS")
                        && !path.contains(&e.id)
                }) {
                    let mut next = path.clone();
                    next.push(edge.id);
                    pending.push((edge.target, next));
                }
            }
            rows.sort();
            Ok(rows)
        }
    }
}
fn property(node: &Node, key: &str) -> Cell {
    match node.properties.get(key) {
        Some(Property::Scalar(Scalar::String(v))) => Cell::String(v.clone()),
        Some(Property::Scalar(Scalar::I64(v))) => Cell::I64(*v),
        Some(Property::Scalar(Scalar::Bool(v))) => Cell::Bool(*v),
        _ => Cell::Null,
    }
}
/// Preserves multiplicity even when logical output order is not specified.
pub fn compare_rows(expected: &[Row], observed: &[Row], ordered: bool) -> Result<(), String> {
    let mut expected = expected.to_vec();
    let mut observed = observed.to_vec();
    if !ordered {
        expected.sort();
        observed.sort();
    }
    if expected == observed {
        Ok(())
    } else {
        Err(format!(
            "PG13 result bag/order mismatch expected={expected:?} observed={observed:?}"
        ))
    }
}

fn score(value: f64) -> Cell {
    Cell::Score(value.to_bits())
}
fn text(node: &Node) -> Cell {
    node.text.clone().map_or(Cell::Null, Cell::String)
}
fn project_meetings(edges: &[&Relationship], project: u128) -> BTreeSet<u128> {
    edges
        .iter()
        .filter(|e| e.relationship_type == "FOR_PROJECT" && e.target == project)
        .map(|e| e.source)
        .collect()
}
fn valid_vector(vector: &[f32]) -> Result<(), String> {
    if vector.is_empty() || vector.iter().any(|v| !v.is_finite()) {
        Err("invalid vector argument".into())
    } else {
        Ok(())
    }
}
/// Scalar ordered f64 accumulation over exact supplied f32 values.
fn distance(bits: &[u32], query: &[f32]) -> Result<f64, String> {
    if bits.len() != query.len() {
        return Err("vector dimension mismatch".into());
    }
    let mut value = 0.0;
    for (b, q) in bits.iter().zip(query) {
        let v = f32::from_bits(*b);
        if !v.is_finite() {
            return Err("nonfinite stored vector".into());
        }
        let delta = f64::from(v) - f64::from(*q);
        value += delta * delta;
    }
    Ok(value)
}
struct Lexical {
    scores: BTreeMap<u128, f64>,
}
/// This reference accepts only the declared lowercase ASCII fixture vocabulary.
/// Rejecting anything else prevents a fake general-purpose analyzer oracle.
fn tokenize(text: &str) -> Result<Vec<&str>, String> {
    if text
        .bytes()
        .any(|b| !b.is_ascii_lowercase() && !b.is_ascii_whitespace())
    {
        return Err("text outside fixture analyzer subset".into());
    }
    Ok(text.split_ascii_whitespace().collect())
}
fn lexical(graph: &Snapshot, terms: &[String], phrase: bool) -> Result<Lexical, String> {
    if terms.is_empty()
        || terms.len() > 64
        || terms
            .iter()
            .any(|t| t.is_empty() || !t.bytes().all(|b| b.is_ascii_lowercase()))
    {
        return Err("invalid fixture query terms".into());
    }
    let mut docs = Vec::new();
    // Unified search statistics include every indexed document, including
    // vector-only and present-empty text rows. Graph-only nodes have no row.
    for node in &graph.nodes {
        if node.text.is_some() || node.vector.is_some() {
            let tokens = node
                .text
                .as_deref()
                .map(tokenize)
                .transpose()?
                .unwrap_or_default();
            docs.push((node.id, tokens));
        }
    }
    let mut scores = BTreeMap::new();
    if docs.is_empty() {
        return Ok(Lexical { scores });
    }
    let n = docs.len() as f64;
    let average = docs.iter().map(|(_, v)| v.len()).sum::<usize>() as f64 / n;
    let unique = terms.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let frequencies = unique
        .iter()
        .map(|term| {
            (
                *term,
                docs.iter()
                    .filter(|(_, tokens)| tokens.contains(term))
                    .count(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for (id, tokens) in docs {
        let mut value = 0.0;
        let has_phrase = !phrase
            || tokens
                .windows(terms.len())
                .any(|window| window.iter().zip(terms).all(|(a, b)| *a == b));
        if has_phrase {
            for term in &unique {
                let tf = tokens.iter().filter(|token| *token == term).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let df = frequencies[term] as f64;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                value +=
                    idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * tokens.len() as f64 / average));
            }
        }
        scores.insert(id, value);
    }
    Ok(Lexical { scores })
}

/// Explicit numeric-only tolerance for comparing f64 reference scores with a
/// product's declared f32 scoring policy. IDs, row order, multiplicity, nulls,
/// modality membership and all non-score values remain exact. Qualification
/// records the chosen tolerances; this function supplies no permissive default.
pub fn compare_scored_rows(
    expected: &[Row],
    observed: &[Row],
    absolute: f64,
    relative: f64,
) -> Result<(), String> {
    if !absolute.is_finite() || !relative.is_finite() || absolute < 0.0 || relative < 0.0 {
        return Err("invalid score comparison tolerance".into());
    }
    if expected.len() != observed.len() {
        return Err("score row multiplicity differs".into());
    }
    for (row, (expected, observed)) in expected.iter().zip(observed).enumerate() {
        if expected.len() != observed.len() {
            return Err(format!("score row width differs at {row}"));
        }
        for (column, (a, b)) in expected.iter().zip(observed).enumerate() {
            let equal = match (a, b) {
                (Cell::Score(a), Cell::Score(b)) => {
                    let a = f64::from_bits(*a);
                    let b = f64::from_bits(*b);
                    a.is_finite() && b.is_finite() && (a - b).abs() <= absolute + relative * a.abs()
                }
                _ => a == b,
            };
            if !equal {
                return Err(format!(
                    "score row differs at {row}/{column}: expected={a:?} observed={b:?}"
                ));
            }
        }
    }
    Ok(())
}
