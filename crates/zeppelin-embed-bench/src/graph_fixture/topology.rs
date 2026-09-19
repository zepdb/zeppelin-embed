use super::*;
fn key(kind: Kind, index: u64) -> NodeKey {
    NodeKey { kind, index }
}
/// Fixed project skew, independent of any engine identity.
pub fn project(config: Config, m: u64) -> u64 {
    if m.is_multiple_of(10) {
        0
    } else {
        1 + m % (config.projects() - 1)
    }
}
/// Five distinct participants, with the explicitly pinned hub/collision rule.
pub fn participants(config: Config, m: u64) -> [u64; 5] {
    let mut people = [0; 5];
    people[0] = if m.is_multiple_of(5) {
        0
    } else {
        1 + (17 * m) % (config.people() - 1)
    };
    for j in 1..5 {
        let mut person = 1 + (31 * m + 97 * j as u64) % (config.people() - 1);
        while people[..j].contains(&person) {
            person = 1 + person % (config.people() - 1);
        }
        people[j] = person;
    }
    people
}
/// Tooling row position, never a promise about an engine-assigned NodeId.
pub fn ordinal(config: Config, key: NodeKey) -> Result<u64, Error> {
    let (value, valid) = match key.kind {
        Kind::Person => (key.index, key.index < config.people()),
        Kind::Project => (config.people() + key.index, key.index < config.projects()),
        Kind::Topic => (
            config.people() + config.projects() + key.index,
            key.index < config.topics(),
        ),
        Kind::Meeting => (
            config.shared() + 27 * key.index,
            key.index < config.meetings(),
        ),
        Kind::Chunk => (
            config.shared() + 27 * (key.index / 20) + 1 + key.index % 20,
            key.index < 20 * config.meetings(),
        ),
        Kind::Decision | Kind::Action => (
            config.shared() + 27 * (key.index / 6) + 21 + key.index % 6,
            key.index < 6 * config.meetings()
                && (key.index.is_multiple_of(2) == (key.kind == Kind::Decision)),
        ),
    };
    if valid {
        Ok(value)
    } else {
        Err("invalid fixture node key".into())
    }
}
pub fn node_key(config: Config, ordinal: u64) -> Result<NodeKey, Error> {
    if ordinal >= config.node_count() {
        return Err("node ordinal outside fixture".into());
    }
    if ordinal < config.people() {
        return Ok(key(Kind::Person, ordinal));
    }
    if ordinal < config.people() + config.projects() {
        return Ok(key(Kind::Project, ordinal - config.people()));
    }
    if ordinal < config.shared() {
        return Ok(key(
            Kind::Topic,
            ordinal - config.people() - config.projects(),
        ));
    }
    let local = ordinal - config.shared();
    let m = local / 27;
    let offset = local % 27;
    Ok(match offset {
        0 => key(Kind::Meeting, m),
        1..=20 => key(Kind::Chunk, m * 20 + offset - 1),
        _ => {
            let index = m * 6 + offset - 21;
            key(
                if index.is_multiple_of(2) {
                    Kind::Decision
                } else {
                    Kind::Action
                },
                index,
            )
        }
    })
}
/// Unbiased bounded selection with no ambient random state.
pub(super) fn choose(words: &mut dyn FnMut() -> u32, count: u64) -> u64 {
    let range = 1_u64 << 32;
    let limit = range - range % count;
    loop {
        let value = u64::from(words());
        if value < limit {
            return value % count;
        }
    }
}
/// Enumerates complete explicit preload/meeting batches. No implicit splitting.
pub fn visit(
    config: Config,
    topics: &mut dyn FnMut() -> u32,
    sink: &mut dyn FnMut(Batch) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut batch = Batch::default();
    for row in 0..config.shared() {
        batch.nodes.push(NodeRecord {
            ordinal: row,
            key: node_key(config, row)?,
            topic: None,
        });
        if batch.nodes.len() == 256 {
            sink(std::mem::take(&mut batch))?;
        }
    }
    if !batch.nodes.is_empty() {
        sink(batch)?;
    }
    for m in 0..config.meetings() {
        sink(meeting(config, m, topics)?)?;
    }
    Ok(())
}
pub(super) fn meeting(
    config: Config,
    m: u64,
    topics: &mut dyn FnMut() -> u32,
) -> Result<Batch, Error> {
    let mut batch = Batch {
        nodes: Vec::with_capacity(27),
        edges: Vec::with_capacity(110),
    };
    let meeting = key(Kind::Meeting, m);
    let project = key(Kind::Project, project(config, m));
    let people = participants(config, m);
    batch.nodes.push(NodeRecord {
        ordinal: ordinal(config, meeting)?,
        key: meeting,
        topic: None,
    });
    for j in 0..20 {
        let node = key(Kind::Chunk, m * 20 + j);
        batch.nodes.push(NodeRecord {
            ordinal: ordinal(config, node)?,
            key: node,
            topic: Some(choose(topics, config.topics())),
        });
    }
    for i in 0..6 {
        let index = m * 6 + i;
        let node = key(
            if i % 2 == 0 {
                Kind::Decision
            } else {
                Kind::Action
            },
            index,
        );
        batch.nodes.push(NodeRecord {
            ordinal: ordinal(config, node)?,
            key: node,
            topic: None,
        });
    }
    let mut edge = |source, target, kind| {
        batch.edges.push(EdgeRecord {
            ordinal: m * 110 + batch.edges.len() as u64,
            source,
            target,
            kind,
        });
    };
    for j in 0..20 {
        edge(meeting, key(Kind::Chunk, m * 20 + j), "HAS_CHUNK");
    }
    for j in 0..20 {
        let chunk = key(Kind::Chunk, m * 20 + j);
        edge(
            chunk,
            key(Kind::Person, people[(j % 5) as usize]),
            "MENTIONS",
        );
        edge(chunk, project, "MENTIONS");
        edge(
            chunk,
            key(
                Kind::Topic,
                batch.nodes[j as usize + 1]
                    .topic
                    .ok_or("missing chunk topic")?,
            ),
            "MENTIONS",
        );
    }
    for i in 0..6 {
        edge(
            meeting,
            key(
                if i % 2 == 0 {
                    Kind::Decision
                } else {
                    Kind::Action
                },
                m * 6 + i,
            ),
            "HAS_ITEM",
        );
    }
    for i in 0..6 {
        edge(
            key(
                if i % 2 == 0 {
                    Kind::Decision
                } else {
                    Kind::Action
                },
                m * 6 + i,
            ),
            key(Kind::Chunk, m * 20 + (m + 3 * i) % 20),
            "SUPPORTED_BY",
        );
    }
    for i in 0..6 {
        let item = key(
            if i % 2 == 0 {
                Kind::Decision
            } else {
                Kind::Action
            },
            m * 6 + i,
        );
        edge(item, key(Kind::Person, people[(i % 5) as usize]), "ABOUT");
        edge(item, project, "ABOUT");
    }
    for person in people {
        edge(key(Kind::Person, person), meeting, "PARTICIPATED_IN");
    }
    edge(meeting, project, "FOR_PROJECT");
    Ok(batch)
}
/// Actual topology/population inventory, retaining only per-node degree counters.
pub fn inventory(config: Config, topics: &mut dyn FnMut() -> u32) -> Result<Summary, Error> {
    let mut summary = Summary::default();
    let mut degree = vec![(0_u64, 0_u64); config.node_count() as usize];
    visit(config, topics, &mut |batch| {
        summary.batches += 1;
        summary.max_batch_changes = summary
            .max_batch_changes
            .max(batch.nodes.len() + batch.edges.len());
        for node in batch.nodes {
            summary.nodes += 1;
            *summary
                .node_kinds
                .entry(node.key.kind.name().into())
                .or_default() += 1;
            let vector = node.key.kind == Kind::Chunk;
            let text = match node.key.kind {
                Kind::Chunk => match node.key.index % 100 {
                    0 => 0,
                    1 => 1,
                    2 => 2,
                    _ => 3,
                },
                Kind::Person | Kind::Project | Kind::Topic => {
                    if node.ordinal % 2 == 0 {
                        3
                    } else {
                        0
                    }
                }
                _ => 3,
            };
            match text {
                0 => summary.absent_text += 1,
                1 => summary.empty_text += 1,
                2 => summary.whitespace_text += 1,
                _ => summary.indexed_text += 1,
            };
            match (vector, text == 3) {
                (true, true) => summary.both += 1,
                (true, false) => summary.vector_only += 1,
                (false, true) => summary.text_only += 1,
                (false, false) => summary.neither += 1,
            };
            if vector {
                summary.vectors += 1;
                summary.vector_bytes += DIMS as u64 * 4;
            }
        }
        for edge in batch.edges {
            summary.edges += 1;
            *summary.edge_kinds.entry(edge.kind.into()).or_default() += 1;
            degree[ordinal(config, edge.source)? as usize].1 += 1;
            degree[ordinal(config, edge.target)? as usize].0 += 1;
        }
        Ok(())
    })?;
    for (incoming, outgoing) in &degree {
        *summary.indegree.entry(*incoming).or_default() += 1;
        *summary.outdegree.entry(*outgoing).or_default() += 1;
        *summary.degree.entry(incoming + outgoing).or_default() += 1;
    }
    let hub = degree[config.people() as usize];
    summary.project_hub_degree = hub.0 + hub.1;
    Ok(summary)
}
