from pathlib import Path
p=Path('crates/zeppelin-embed/src/property_graph/query/plan/mod.rs');s=p.read_text()
old='''        /// Optional symbolic type constraint.
        relationship_type: Option<GraphName<'a>>,'''
new='''        /// Exact-name OR alternatives; empty means unrestricted. Duplicate names
        /// never multiply runtime rows. Resolution uses the same admitted catalog.
        relationship_types: &'a [GraphName<'a>],'''
assert s.count(old)==2;s=s.replace(old,new)
at='''        /// New relationship-list slot, including for one hop.
        relationships: SlotId,''';assert s.count(at)==1;s=s.replace(at,at+'''
        /// Predicate over each candidate edge before it enters a path. Null does
        /// not match; zero-hop paths evaluate no edges. The private slot never
        /// appears in this operator's output schema.
        edge_predicate: Option<EdgePredicate>,''')
at='''/// One ORDER BY key; ties remain ties unless a later key distinguishes them.'''
s=s.replace(at,'''/// Candidate-edge scope for bounded traversal, separate from its public list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdgePredicate {
    /// Fresh nonnullable relationship slot visible only while evaluating this
    /// predicate, alongside the traversal's input bindings. It must differ from
    /// every input slot and both new traversal output slots.
    pub current_edge: SlotId,
    /// Boolean/null expression in the private candidate-edge scope.
    pub expression: ExprId,
}
''' +at,1)
at='''    /// Possible value kinds for a slot in this exact scope.
    pub fn slot(&self, id: SlotId) -> Option<ValueKinds> {''';assert s.count(at)==1;s=s.replace(at,'''    /// Read-only ordinal schema access; logical SlotId is never a cell offset.
    pub fn slot_at(&self, ordinal: usize) -> Option<(SlotId, ValueKinds)> {
        let slot = self.slots.get(..self.width)?.get(ordinal)?;
        Some((SlotId(slot.id), slot.kinds))
    }
''' +at)
p.write_text(s)
p=Path('crates/zeppelin-embed/src/property_graph/query/plan/validate.rs');s=p.read_text();old='''            OperatorKind::Expand {
                relationship_type: Some(name),
                ..
            }
            | OperatorKind::BoundedExpand {
                relationship_type: Some(name),
                ..
            } => accounting.span(name.as_str().as_bytes(), context)?,''';new='''            OperatorKind::Expand {
                relationship_types, ..
            }
            | OperatorKind::BoundedExpand {
                relationship_types, ..
            } => {
                accounting.span(relationship_types, context)?;
                for name in relationship_types {
                    context.step()?;
                    accounting.span(name.as_str().as_bytes(), context)?;
                }
            }''';assert old in s;s=s.replace(old,new)
old='''            relationships,
            min,
            max,
            ..
        } => {
            if min > max || max > 16 {
                return Err(PlanError::PathBound);
            }
            expand(&mut output, source, node, relationships, ValueKinds::LIST)?;
        }''';new='''            relationships,
            edge_predicate,
            min,
            max,
            ..
        } => {
            if min > max || max > 16 {
                return Err(PlanError::PathBound);
            }
            expand(&mut output, source, node, relationships, ValueKinds::LIST)?;
            if let Some(predicate) = edge_predicate {
                if predicate.current_edge == node || predicate.current_edge == relationships {
                    return Err(PlanError::Scope);
                }
                let mut scope = input.clone();
                add_slot(&mut scope, Slot {
                    id: predicate.current_edge.0,
                    kinds: ValueKinds::REL,
                })?;
                boolean(description, predicate.expression, &scope, seen, context)?;
            }
        }''';assert old in s;s=s.replace(old,new);p.write_text(s)
p=Path('crates/zeppelin-embed/tests/graph_query_plan.rs');s=p.read_text().replace('relationship_type: None','relationship_types: &[]')
old='''            | OperatorKind::Expand {
                relationship_type: Some(name),
                ..
            }
            | OperatorKind::BoundedExpand {
                relationship_type: Some(name),
                ..
            }
''';assert old in s;s=s.replace(old,'')
at='''            OperatorKind::Mutate(items) => {
                add(&mut regions, items);''';new='''            OperatorKind::Expand { relationship_types, .. }
            | OperatorKind::BoundedExpand { relationship_types, .. } => {
                add(&mut regions, relationship_types);
                for name in relationship_types {
                    add(&mut regions, name.as_str().as_bytes());
                }
            }
'''+at;assert at in s;s=s.replace(at,new)
start=0
while True:
 at=s.find('OperatorKind::BoundedExpand {',start)
 if at<0:break
 brace=s.index('{',at);pos=brace+1;depth=1
 while depth:
  if s[pos]=='{':depth+=1
  elif s[pos]=='}':depth-=1
  pos+=1
 block=s[brace+1:pos-1]
 if 'min:' in block and 'edge_predicate' not in block:
  mark=s.index('relationship_types:',brace,pos);line=s.rfind('\n',brace,mark)+1;indent=s[line:mark];s=s[:line]+indent+'edge_predicate: None,\n'+s[line:];pos+=len(indent)+len('edge_predicate: None,\n')
 start=pos
old='''    let OperatorKind::BoundedExpand { relationship_types, edge_predicate, .. } = operators[3].kind else { unreachable!() };''';new='''    let (relationship_types, edge_predicate) = match operators[3].kind {
        OperatorKind::BoundedExpand { relationship_types, edge_predicate, .. } => Some((relationship_types, edge_predicate)),
        _ => None,
    }.unwrap();''';assert old in s;s=s.replace(old,new);p.write_text(s)
