use super::*;
use zeppelin_embed::property_graph::query::{
    MAX_LIST_DEPTH, MAX_LIST_ELEMENTS, MAX_QUERY_BYTES, QueryList,
};
impl Binder<'_, '_> {
    pub(super) fn parameters(&mut self) -> Result<(), ParseError> {
        if self.parameters.len() > self.limits.parameters {
            return Err(ParseError::new(
                ErrorKind::Limit(LimitKind::Parameters),
                Span::default(),
                "parameter limit",
            ));
        }
        for (index, parameter) in self.parameters.iter().enumerate() {
            poll(self.resources, Span::default())?;
            // A supplied name longer than the entire accepted query cannot
            // occur in it. Reject before comparing attacker-owned long names.
            if parameter.name.len() > self.limits.text_bytes {
                return Err(ParseError::new(
                    ErrorKind::Parameter,
                    Span::default(),
                    "surplus parameter name",
                ));
            }
            for prior in self
                .parameters
                .get(..index)
                .ok_or_else(|| error(Span::default(), "parameter index"))?
            {
                poll(self.resources, Span::default())?;
                if prior.name == parameter.name {
                    return Err(ParseError::new(
                        ErrorKind::Parameter,
                        Span::default(),
                        "duplicate parameter",
                    ));
                }
            }
            let mut span = None;
            for node in self.ast.nodes() {
                poll(self.resources, node.span)?;
                if let NodeKind::Parameter(name) = node.kind
                    && self.ast.text(name) == Some(parameter.name)
                {
                    span = Some(node.span);
                }
            }
            let span = span.ok_or_else(|| {
                ParseError::new(ErrorKind::Parameter, Span::default(), "surplus parameter")
            })?;
            self.parameter_value(parameter.value, span)?;
        }
        Ok(())
    }
    fn parameter_value(&mut self, value: QueryValue<'_>, span: Span) -> Result<(), ParseError> {
        let mut stack: [Option<(QueryList<'_>, usize)>; MAX_LIST_DEPTH as usize] =
            [None; MAX_LIST_DEPTH as usize];
        let mut depth = 0_usize;
        let mut next = Some(value);
        let mut elements = 0_usize;
        loop {
            poll(self.resources, span)?;
            if let Some(value) = next.take() {
                match value {
                    QueryValue::NodeRef(_) | QueryValue::RelRef(_) => {
                        return Err(ParseError::new(
                            ErrorKind::Parameter,
                            span,
                            "entity parameter, including a nested list",
                        ));
                    }
                    QueryValue::String(value) => {
                        if value.len() > MAX_QUERY_BYTES {
                            return Err(ParseError::new(
                                ErrorKind::Parameter,
                                span,
                                "parameter string exceeds query envelope",
                            ));
                        }
                        for _ in value.as_bytes().chunks(65536) {
                            poll(self.resources, span)?;
                        }
                    }
                    QueryValue::List(list) => {
                        if depth >= self.limits.list_depth || list.elements() > MAX_LIST_ELEMENTS {
                            return Err(ParseError::new(
                                ErrorKind::Limit(LimitKind::ListDepth),
                                span,
                                "parameter list geometry",
                            ));
                        }
                        *stack
                            .get_mut(depth)
                            .ok_or_else(|| error(span, "parameter stack bound"))? = Some((list, 0));
                        depth += 1;
                    }
                    _ => {}
                }
            }
            loop {
                if depth == 0 {
                    return Ok(());
                }
                let frame = stack
                    .get_mut(depth - 1)
                    .and_then(Option::as_mut)
                    .ok_or_else(|| error(span, "parameter frame"))?;
                if frame.1 == frame.0.len() {
                    depth -= 1;
                    continue;
                }
                elements = elements
                    .checked_add(1)
                    .ok_or_else(|| error(span, "parameter element count overflow"))?;
                if elements > MAX_LIST_ELEMENTS {
                    return Err(ParseError::new(
                        ErrorKind::Parameter,
                        span,
                        "parameter element limit",
                    ));
                }
                next = Some(
                    frame
                        .0
                        .get(frame.1)
                        .ok_or_else(|| error(span, "parameter element"))?,
                );
                frame.1 += 1;
                break;
            }
        }
    }
}
