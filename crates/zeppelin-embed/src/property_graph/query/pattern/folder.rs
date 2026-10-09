//! Exact bitmap count, corrected only at explicit graph records.
use super::*;

impl<'s, 'r, 'plan, 'v, 'm, 'g, 'i, 'q> NativePattern<'s, 'r, 'plan, 'v, 'm, 'g, 'i, 'q> {
    #[allow(clippy::result_large_err)]
    pub(super) fn folder_count(
        &mut self,
        index: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Option<i64>, NativeExecutionError> {
        let node = self
            .occurrences
            .as_slice()
            .get(index)
            .ok_or(RuntimeError::Batch)?
            .node;
        let Some(folder) = self.folder.filter(|folder| folder.count == Some(node)) else {
            return Ok(None);
        };
        let Some(candidates) = self.view.folder_candidates(folder.value, context)? else {
            return Ok(None);
        };
        let mut count = candidates.cardinality()?;
        let scan = self
            .occurrences
            .as_slice()
            .iter()
            .position(|op| op.node == folder.scan)
            .ok_or(RuntimeError::Batch)?;
        if self
            .occurrences
            .as_slice()
            .get(scan)
            .ok_or(RuntimeError::Batch)?
            .schema
            .slots()
            != [folder.slot]
        {
            return Err(RuntimeError::Batch.into());
        }
        let mut after = None;
        loop {
            let next = {
                let mut resources = TreeResources::for_query(context)?;
                self.view.next_folder_correction(after, &mut resources)?
            };
            let Some((node, live)) = next else {
                break;
            };
            after = Some(node);
            // Document attributes have precedence over graph properties.
            if self.view.document_property(node, "folder")?
                == Some(crate::meta::PredicateValue::U64(folder.value))
            {
                count = count.checked_sub(1).ok_or(RuntimeError::Batch)?;
            }
            if !live {
                continue;
            }
            let occurrence = self
                .occurrences
                .as_mut_slice()
                .get_mut(scan)
                .ok_or(RuntimeError::Batch)?;
            occurrence.output.clear();
            occurrence
                .output
                .push_row(&[self.query_view.node(node)], context)?;
            let mut matched = true;
            for expression in [folder.label, folder.predicate] {
                let occurrence = self
                    .occurrences
                    .as_slice()
                    .get(scan)
                    .ok_or(RuntimeError::Batch)?;
                let value = evaluate_at(
                    &mut self.evaluator,
                    None,
                    expression,
                    &occurrence.schema,
                    &occurrence.output,
                    0,
                    self.view,
                    context,
                )?;
                if !value.truth().map_err(RuntimeError::Value)?.retained() {
                    matched = false;
                    break;
                }
            }
            if matched {
                count = count.checked_add(1).ok_or(RuntimeError::Batch)?;
            }
        }
        Ok(Some(i64::try_from(count).map_err(|_| RuntimeError::Batch)?))
    }
}
