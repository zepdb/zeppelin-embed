//! Bounded nonentity parameter backing. Each list depth owns its own arena.
use super::{Pool, invalid};
use crate::{ZeErrorCode, ZeGraphParameterValue, error::FfiError};
use zeppelin_embed::lifecycle::QueryControl;
use zeppelin_embed::property_graph::query::plan::ParameterBinding;
use zeppelin_embed::property_graph::query::{
    MAX_VALUE_WORK, QueryList, QueryValue, QueryView, ValueContext,
};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

#[derive(Clone, Copy)]
enum Value<'a> {
    Scalar(QueryValue<'a>),
    List { start: usize, count: usize },
}
fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<(), FfiError> {
    values
        .try_reserve_exact(additional)
        .map_err(|_| FfiError::new(ZeErrorCode::ZeErrOutOfMemory, "graph parameter backing"))
}
fn copy<'p>(
    pool: &Pool<'p>,
    index: u32,
    level: usize,
    levels: &mut [Vec<Value<'p>>],
    ancestors: &mut Vec<u32>,
    elements: &mut usize,
) -> Result<Value<'p>, FfiError> {
    let value = pool.value(index, "parameter")?;
    Ok(match value.tag {
        0 => Value::Scalar(QueryValue::Null),
        1 => Value::Scalar(QueryValue::Bool(value.boolean == 1)),
        2 => Value::Scalar(QueryValue::I64(value.integer)),
        3 => Value::Scalar(QueryValue::F64(value.floating)),
        4 => Value::Scalar(QueryValue::String(
            pool.text(value.range, "parameter string")?,
        )),
        7 => {
            if level >= 16 || ancestors.contains(&index) {
                return Err(invalid("cyclic or over-depth parameter list"));
            }
            let children = pool.children(value.range)?;
            *elements = elements
                .checked_add(children.len())
                .ok_or_else(|| invalid("list element overflow"))?;
            if *elements > 524_288 {
                return Err(invalid("parameter lists exceed 524288 elements"));
            }
            let next = levels
                .get_mut(level + 1)
                .ok_or_else(|| invalid("list depth"))?;
            let start = next.len();
            reserve(next, children.len())?;
            next.resize(start + children.len(), Value::Scalar(QueryValue::Null));
            ancestors.push(index);
            for (offset, child) in children.iter().enumerate() {
                if value.list_kind != 0 && value.list_kind != 5 {
                    let expected = value.list_kind;
                    if pool.value(*child, "typed list element")?.tag != expected {
                        return Err(invalid("typed parameter list kind mismatch"));
                    }
                }
                let decoded = copy(pool, *child, level + 1, levels, ancestors, elements)?;
                *levels
                    .get_mut(level + 1)
                    .and_then(|v| v.get_mut(start + offset))
                    .ok_or_else(|| invalid("list backing index"))? = decoded;
            }
            ancestors.pop();
            Value::List {
                start,
                count: children.len(),
            }
        }
        _ => {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrParameter,
                "parameters cannot contain entities",
            ));
        }
    })
}
fn freeze<R>(
    levels: &[Vec<Value<'_>>],
    depth: usize,
    children: &[QueryValue<'_>],
    context: &mut ValueContext<'_>,
    run: &mut dyn FnMut(&[QueryValue<'_>]) -> Result<R, FfiError>,
) -> Result<R, FfiError> {
    let source = levels
        .get(depth)
        .ok_or_else(|| invalid("parameter depth"))?;
    let mut output = Vec::new();
    reserve(&mut output, source.len())?;
    for value in source {
        output.push(match *value {
            Value::Scalar(value) => value,
            Value::List { start, count } => {
                let values = children
                    .get(start..start + count)
                    .ok_or_else(|| invalid("parameter list span"))?;
                QueryValue::List(
                    QueryList::new(values, context)
                        .map_err(|e| FfiError::new(e.into(), format!("parameter list: {e}")))?,
                )
            }
        });
    }
    if depth == 0 {
        run(&output)
    } else {
        freeze(levels, depth - 1, &output, context, run)
    }
}
pub(super) fn with_parameters_accounted<R>(
    bindings: &[ZeGraphParameterValue],
    pool: Option<&Pool<'_>>,
    control: &QueryControl,
    run: impl FnOnce(&[ParameterBinding<'_>], usize) -> Result<R, FfiError>,
) -> Result<R, FfiError> {
    if bindings.len() > 256 {
        return Err(invalid("more than 256 parameters"));
    }
    if bindings.is_empty() {
        return run(&[], 0);
    }
    let pool = pool.ok_or_else(|| invalid("parameters require a pool"))?;
    let mut levels: [Vec<Value<'_>>; 17] = std::array::from_fn(|_| Vec::new());
    let mut names = Vec::new();
    reserve(&mut names, bindings.len())?;
    reserve(
        levels.get_mut(0).ok_or_else(|| invalid("parameter root"))?,
        bindings.len(),
    )?;
    let mut ancestors = Vec::new();
    reserve(&mut ancestors, 16)?;
    let mut elements = 0;
    for binding in bindings {
        binding
            .validate_header()
            .map_err(|_| invalid("parameter header"))?;
        if binding.reserved != 0 {
            return Err(invalid("parameter reserved field"));
        }
        let name = pool.text(binding.name, "parameter name")?;
        if names.contains(&name) {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrParameter,
                "duplicate parameter name",
            ));
        }
        names.push(name);
        let value = copy(
            pool,
            binding.value,
            0,
            &mut levels,
            &mut ancestors,
            &mut elements,
        )?;
        levels
            .get_mut(0)
            .ok_or_else(|| invalid("parameter root"))?
            .push(value);
    }
    // This token carries no store authority. Entity descendants were rejected.
    let view = QueryView::new(
        StoreInstanceId::new(1).map_err(|_| invalid("parameter value token"))?,
        GraphGeneration::new(0),
    );
    let mut context = ValueContext::new(&view, control, MAX_VALUE_WORK)
        .map_err(|e| FfiError::new(e.into(), e.to_string()))?;
    let bytes = levels
        .iter()
        .try_fold(0usize, |total, level| {
            total.checked_add(
                level.capacity()
                    * (std::mem::size_of::<Value<'_>>() + std::mem::size_of::<QueryValue<'_>>()),
            )
        })
        .and_then(|total| total.checked_add(names.capacity() * std::mem::size_of::<&str>()))
        .and_then(|total| total.checked_add(ancestors.capacity() * std::mem::size_of::<u32>()))
        .ok_or_else(|| invalid("parameter backing capacity overflow"))?;
    let mut run = Some(run);
    freeze(&levels, 16, &[], &mut context, &mut |values| {
        let mut decoded = Vec::new();
        reserve(&mut decoded, bindings.len())?;
        for (name, value) in names.iter().zip(values) {
            decoded.push(ParameterBinding {
                name,
                value: *value,
            });
        }
        run.take()
            .ok_or_else(|| invalid("parameter consumer called twice"))?(
            &decoded,
            bytes + decoded.capacity() * std::mem::size_of::<ParameterBinding<'_>>(),
        )
    })
}

#[cfg(test)]
fn with_parameters<R>(
    bindings: &[ZeGraphParameterValue],
    pool: Option<&Pool<'_>>,
    control: &QueryControl,
    run: impl FnOnce(&[ParameterBinding<'_>]) -> Result<R, FfiError>,
) -> Result<R, FfiError> {
    with_parameters_accounted(bindings, pool, control, |bindings, _| run(bindings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    fn sized<T>() -> T {
        // These descriptors have only integer, floating, range and pointer fields.
        let mut value: T = unsafe { std::mem::zeroed() };
        unsafe {
            (&mut value as *mut T)
                .cast::<u32>()
                .write(std::mem::size_of::<T>() as u32);
        }
        value
    }
    #[test]
    #[allow(clippy::unwrap_used, clippy::indexing_slicing)]
    fn ze72_nested_null_and_empty_query_parameters_preserve_shape() {
        let mut values: [ZeGraphValue; 4] = [sized(); 4];
        values[1].tag = 7;
        values[2].tag = 7;
        values[2].range = ZeGraphRange { start: 0, count: 2 };
        values[3].tag = 7;
        values[3].range = ZeGraphRange { start: 2, count: 1 };
        let children = [0, 1, 2];
        let mut raw: ZeGraphValuePool = sized();
        raw.bytes = b"p".as_ptr();
        raw.byte_count = 1;
        raw.values = values.as_ptr();
        raw.value_count = values.len();
        raw.children = children.as_ptr();
        raw.child_count = children.len();
        let mut binding: ZeGraphParameterValue = sized();
        binding.name.count = 1;
        binding.value = 3;
        let control = QueryControl::Cancel(zeppelin_embed::lifecycle::CancelToken::new());
        let pool = Pool::read(&raw, "ZE-72 nested parameters").unwrap();
        with_parameters(&[binding], Some(&pool), &control, |bindings| {
            let QueryValue::List(outer) = bindings[0].value else {
                return Err(invalid("outer list lost"));
            };
            let Some(QueryValue::List(inner)) = outer.get(0) else {
                return Err(invalid("inner list lost"));
            };
            assert_eq!(inner.len(), 2);
            assert!(matches!(inner.get(0), Some(QueryValue::Null)));
            assert!(matches!(inner.get(1), Some(QueryValue::List(empty)) if empty.is_empty()));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn ze241_raw_lists_reject_cycles_indices_and_entities() {
        let mut value: ZeGraphValue = sized();
        value.tag = 7;
        value.range.count = 1;
        let mut children = [0];
        let mut raw: ZeGraphValuePool = sized();
        raw.bytes = b"p".as_ptr();
        raw.byte_count = 1;
        raw.values = &value;
        raw.value_count = 1;
        raw.children = children.as_ptr();
        raw.child_count = 1;
        let mut binding: ZeGraphParameterValue = sized();
        binding.name.count = 1;
        let control = QueryControl::Cancel(zeppelin_embed::lifecycle::CancelToken::new());
        let pool = Pool::read(&raw, "raw lists").unwrap();
        assert!(with_parameters(&[binding], Some(&pool), &control, |_| Ok(())).is_err());
        children[0] = 7;
        raw.children = children.as_ptr();
        let pool = Pool::read(&raw, "raw lists").unwrap();
        assert!(with_parameters(&[binding], Some(&pool), &control, |_| Ok(())).is_err());
        value.range.count = 0;
        raw.values = &value;
        let pool = Pool::read(&raw, "raw lists").unwrap();
        with_parameters(&[binding], Some(&pool), &control, |bindings| {
            assert!(matches!(bindings.first().map(|b| b.value), Some(QueryValue::List(list)) if list.is_empty()));
            Ok(())
        }).unwrap();
        value.tag = 5;
        raw.values = &value;
        let pool = Pool::read(&raw, "raw lists").unwrap();
        assert!(with_parameters(&[binding], Some(&pool), &control, |_| Ok(())).is_err());
    }
}
