//! Bookkeeping projections require identity ownership, not entity liveness.
use super::{QueryError, QueryValue, ValueContext};
impl QueryValue<'_> {
    /// Formats all node identity bits into exactly 32 caller-reserved bytes.
    /// Null propagates and wrong-kind/foreign-view inputs leave output unchanged.
    pub fn node_id_text<'a>(
        self,
        output: &'a mut [u8; 32],
        context: &mut ValueContext<'_>,
    ) -> Result<QueryValue<'a>, QueryError> {
        context.step()?;
        self.validate(context)?;
        match self {
            Self::Null => Ok(QueryValue::Null),
            Self::NodeRef(value) => format_id(value.id().get(), output, context),
            _ => Err(QueryError::Type),
        }
    }
    /// Formats a relationship identity without reinterpreting it as a node.
    pub fn relationship_id_text<'a>(
        self,
        output: &'a mut [u8; 32],
        context: &mut ValueContext<'_>,
    ) -> Result<QueryValue<'a>, QueryError> {
        context.step()?;
        self.validate(context)?;
        match self {
            Self::Null => Ok(QueryValue::Null),
            Self::RelRef(value) => format_id(value.id().get(), output, context),
            _ => Err(QueryError::Type),
        }
    }
}
fn format_id<'a>(
    id: u128,
    output: &'a mut [u8; 32],
    context: &mut ValueContext<'_>,
) -> Result<QueryValue<'a>, QueryError> {
    context.output(32)?;
    for (index, byte) in output.iter_mut().enumerate() {
        let digit = ((id >> ((31 - index) * 4)) & 15) as u8;
        *byte = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
    }
    Ok(QueryValue::String(
        std::str::from_utf8(output).map_err(|_| QueryError::Type)?,
    ))
}
