use super::super::QueryValue;
use super::super::value::compare_bytes;
use super::*;
use std::cmp::Ordering;
pub(super) fn validate(
    parameters: &[Parameter<'_>],
    bindings: &[ParameterBinding<'_>],
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    context.step()?;
    if parameters.len() != bindings.len() {
        return Err(PlanError::Parameter);
    }
    for parameter in parameters {
        let mut found = false;
        for binding in bindings {
            context.step()?;
            if compare_bytes(parameter.name.as_bytes(), binding.name.as_bytes(), context)?
                != Ordering::Equal
            {
                continue;
            }
            if found || binding.value.view().is_some() {
                return Err(PlanError::Parameter);
            }
            found = true;
            binding.value.validate(context)?;
            let kind = match binding.value {
                QueryValue::Null => ValueKinds::NULL,
                QueryValue::Bool(_) => ValueKinds::BOOL,
                QueryValue::I64(_) => ValueKinds::I64,
                QueryValue::F64(_) => ValueKinds::F64,
                QueryValue::String(_) => ValueKinds::STRING,
                QueryValue::List(_) => ValueKinds::LIST,
                QueryValue::NodeRef(_) | QueryValue::RelRef(_) => return Err(PlanError::Parameter),
            };
            if !parameter.kinds.contains(kind) {
                return Err(PlanError::Parameter);
            }
        }
        if !found {
            return Err(PlanError::Parameter);
        }
    }
    Ok(())
}
