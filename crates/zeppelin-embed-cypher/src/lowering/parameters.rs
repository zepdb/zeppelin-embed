use super::*;
use zeppelin_embed::property_graph::query::{MAX_LIST_DEPTH, QueryList, QueryValue};

#[derive(Clone, Copy)]
pub(super) enum ParameterValue {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(Range),
    List(Range),
}
#[derive(Clone, Copy)]
struct Pending<'a> {
    value: QueryValue<'a>,
    level: usize,
    index: usize,
}
impl<'m, 'g> Builder<'m, 'g, '_> {
    fn level(&self, depth: usize) -> Result<&Buffer<'m, 'g, ParameterValue>, ParseError> {
        self.parameter_levels
            .get(depth)
            .and_then(Option::as_ref)
            .ok_or_else(|| invariant(Span::default(), "parameter depth owner"))
    }
    fn level_mut(
        &mut self,
        depth: usize,
    ) -> Result<&mut Buffer<'m, 'g, ParameterValue>, ParseError> {
        self.parameter_levels
            .get_mut(depth)
            .and_then(Option::as_mut)
            .ok_or_else(|| invariant(Span::default(), "parameter depth owner"))
    }
    /// Iterative level-indexed copying; no nested heap owners or recursive drops.
    pub(super) fn copy_parameters(
        &mut self,
        parameters: &[ParameterBinding<'_>],
    ) -> Result<(), ParseError> {
        let memory = self.memory;
        let control = self.control;
        for level in &mut self.parameter_levels {
            *level = Some(Buffer::new(memory)?);
        }
        let mut pending = Buffer::new(memory)?;
        for parameter in parameters {
            let name = self.copy_text(parameter.name)?;
            self.parameter_names.push(name, memory, control)?;
            let index = self.level(0)?.len();
            self.level_mut(0)?
                .push(ParameterValue::Null, memory, control)?;
            pending.push(
                Pending {
                    value: parameter.value,
                    level: 0,
                    index,
                },
                memory,
                control,
            )?;
        }
        let mut next = 0;
        while next < pending.len() {
            check(control, Span::default())?;
            let item = *pending
                .slice()
                .get(next)
                .ok_or_else(|| invariant(Span::default(), "parameter copy frame"))?;
            next += 1;
            let value = match item.value {
                QueryValue::Null => ParameterValue::Null,
                QueryValue::Bool(v) => ParameterValue::Bool(v),
                QueryValue::I64(v) => ParameterValue::I64(v),
                QueryValue::F64(v) => ParameterValue::F64(v),
                QueryValue::String(v) => ParameterValue::String(self.copy_text(v)?),
                QueryValue::List(list) => {
                    if item.level >= MAX_LIST_DEPTH as usize {
                        return Err(ParseError::new(
                            ErrorKind::Limit(LimitKind::ListDepth),
                            Span::default(),
                            "parameter list depth",
                        ));
                    }
                    let start = self.level(item.level + 1)?.len();
                    for index in 0..list.len() {
                        check(control, Span::default())?;
                        let value = list
                            .get(index)
                            .ok_or_else(|| invariant(Span::default(), "parameter list element"))?;
                        self.level_mut(item.level + 1)?.push(
                            ParameterValue::Null,
                            memory,
                            control,
                        )?;
                        pending.push(
                            Pending {
                                value,
                                level: item.level + 1,
                                index: start + index,
                            },
                            memory,
                            control,
                        )?;
                    }
                    ParameterValue::List(Range {
                        start,
                        len: list.len(),
                    })
                }
                _ => {
                    return Err(ParseError::new(
                        ErrorKind::Parameter,
                        Span::default(),
                        "entity parameter",
                    ));
                }
            };
            *self
                .level_mut(item.level)?
                .arena
                .as_mut_slice()
                .get_mut(item.index)
                .ok_or_else(|| invariant(Span::default(), "parameter output index"))? = value;
        }
        Ok(())
    }
    /// Each depth owns separate actual backing, allowing safe immutable children
    /// without self-referential vectors or unsafe lifetime extension.
    pub(super) fn freeze_level<'a>(
        &'a self,
        depth: usize,
        children: &'a [QueryValue<'a>],
        context: &mut ValueContext<'_>,
    ) -> Result<QueryArena<'m, 'g, QueryValue<'a>>, ParseError> {
        let source = self.level(depth)?;
        let mut output = QueryArena::new(self.memory, source.len()).map_err(memory_error)?;
        for value in source.slice() {
            check(self.control, Span::default())?;
            let value = match *value {
                ParameterValue::Null => QueryValue::Null,
                ParameterValue::Bool(v) => QueryValue::Bool(v),
                ParameterValue::I64(v) => QueryValue::I64(v),
                ParameterValue::F64(v) => QueryValue::F64(v),
                ParameterValue::String(range) => {
                    QueryValue::String(text(self.bytes.slice(), range, self.control)?)
                }
                ParameterValue::List(range) => QueryValue::List(
                    QueryList::new(range.get(children)?, context).map_err(|e| {
                        ParseError::new(
                            ErrorKind::Resource(context::resource_error(e)),
                            Span::default(),
                            "copied parameter list",
                        )
                    })?,
                ),
            };
            output.push(value).map_err(memory_error)?;
        }
        Ok(output)
    }
}
pub(super) fn kind(value: QueryValue<'_>) -> ValueKinds {
    match value {
        QueryValue::Null => ValueKinds::NULL,
        QueryValue::Bool(_) => ValueKinds::BOOL,
        QueryValue::I64(_) => ValueKinds::I64,
        QueryValue::F64(_) => ValueKinds::F64,
        QueryValue::String(_) => ValueKinds::STRING,
        QueryValue::List(_) => ValueKinds::LIST,
        QueryValue::NodeRef(_) => ValueKinds::NODE,
        QueryValue::RelRef(_) => ValueKinds::REL,
    }
}
