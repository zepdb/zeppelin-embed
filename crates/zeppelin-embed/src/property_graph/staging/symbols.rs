use super::*;
use catalog::SymbolEntry;
use memory::Arena;
#[derive(Clone, Copy)]
struct Candidate<'a> {
    kind: SymbolKind,
    name: GraphName<'a>,
}

pub(super) fn prepare<'a, 'batch, I>(
    base: &dyn AdmittedBase,
    inputs: I,
    high: &mut HighWaters,
    memory: &'a WriteMemory<'a>,
    control: &mut WriteControl<'_>,
) -> Result<Arena<'a, SymbolEntry<'a>>, StageError>
where
    I: Iterator<Item = (Option<ApplicationKey<'a>>, Option<WriteImage<'a, 'batch>>)> + Clone,
{
    let mut capacity = 0usize;
    for (key, image) in inputs.clone() {
        control(WritePhase::Validate)?;
        let names = match image {
            Some(WriteImage::Node(image)) => {
                let (l, p, _, _) = image.staging_node_parts().ok_or(StageError::InvalidInput)?;
                l.len().checked_add(p.len()).ok_or(StageError::Limit)?
            }
            Some(WriteImage::Relationship { properties, .. }) => {
                properties.len().checked_add(1).ok_or(StageError::Limit)?
            }
            None => 0,
        };
        capacity = capacity
            .checked_add(names)
            .and_then(|n| n.checked_add(usize::from(key.is_some())))
            .ok_or(StageError::Limit)?;
        if capacity > MAX_GRAPH_INPUT_BYTES / 8 {
            return Err(StageError::Limit);
        }
    }
    let mut candidates = Arena::new(memory, capacity, control)?;
    for (key, image) in inputs {
        control(WritePhase::Validate)?;
        if let Some(key) = key {
            candidates.push(Candidate {
                kind: SymbolKind::Namespace,
                name: key.namespace(),
            })?;
        }
        match image {
            Some(WriteImage::Node(image)) => {
                let (labels, properties, _, _) =
                    image.staging_node_parts().ok_or(StageError::InvalidInput)?;
                for label in labels {
                    control(WritePhase::Validate)?;
                    candidates.push(Candidate {
                        kind: SymbolKind::Label,
                        name: *label,
                    })?;
                }
                for property in properties {
                    control(WritePhase::Validate)?;
                    candidates.push(Candidate {
                        kind: SymbolKind::Property,
                        name: property.name(),
                    })?;
                }
            }
            Some(WriteImage::Relationship {
                relationship_type,
                properties,
                ..
            }) => {
                candidates.push(Candidate {
                    kind: SymbolKind::RelationshipType,
                    name: relationship_type,
                })?;
                for property in properties {
                    control(WritePhase::Validate)?;
                    candidates.push(Candidate {
                        kind: SymbolKind::Property,
                        name: property.name(),
                    })?;
                }
            }
            None => {}
        }
    }
    bounded::sort(candidates.as_mut_slice(), control, |a, b, c| {
        let kind = (a.kind as u8).cmp(&(b.kind as u8));
        if !kind.is_eq() {
            return Ok(kind);
        }
        bounded::bytes(a.name.as_str().as_bytes(), b.name.as_str().as_bytes(), c)
    })?;
    let mut additions = Arena::new(memory, capacity, control)?;
    let mut previous: Option<Candidate<'a>> = None;
    for candidate in &*candidates {
        control(WritePhase::Validate)?;
        if let Some(old) = previous
            && old.kind == candidate.kind
            && bounded::bytes(
                old.name.as_str().as_bytes(),
                candidate.name.as_str().as_bytes(),
                control,
            )?
            .is_eq()
        {
            continue;
        }
        previous = Some(*candidate);
        if let Some(symbol) = base.symbol(candidate.kind, candidate.name, control)? {
            if symbol.kind() != candidate.kind
                || symbol.get() > base.high_waters().symbols.get(candidate.kind)
            {
                return Err(StageError::ViewMismatch);
            }
            continue;
        }
        let next = high
            .symbols
            .get(candidate.kind)
            .checked_add(1)
            .ok_or(catalog::CatalogError::SymbolOverflow)?;
        let symbol = Symbol::new(candidate.kind, next)?;
        match candidate.kind {
            SymbolKind::Label => high.symbols.label = next,
            SymbolKind::RelationshipType => high.symbols.relationship_type = next,
            SymbolKind::Property => high.symbols.property = next,
            SymbolKind::Namespace => high.symbols.namespace = next,
        }
        additions.push(SymbolEntry {
            symbol,
            name: candidate.name,
        })?;
    }
    Ok(additions)
}
