use super::*;

/// Grow by replacing a real arena while both allocations remain charged.
/// No Vec capacity or copied payload is inferred from a borrowed slice.
pub(super) struct Buffer<'m, 'g, T> {
    pub arena: QueryArena<'m, 'g, T>,
}
impl<'m, 'g, T: Copy> Buffer<'m, 'g, T> {
    pub fn new(memory: &'m QueryMemory<'g>) -> Result<Self, ParseError> {
        Ok(Self {
            arena: QueryArena::new(memory, 0).map_err(memory_error)?,
        })
    }
    pub fn push(
        &mut self,
        value: T,
        memory: &'m QueryMemory<'g>,
        control: &dyn Fn() -> Result<(), ResourceError>,
    ) -> Result<(), ParseError> {
        check(control, Span::default())?;
        if self.arena.len() == self.arena.capacity() {
            let capacity = self
                .arena
                .capacity()
                .max(4)
                .checked_mul(2)
                .ok_or_else(|| limit(Span::default()))?;
            let mut replacement = QueryArena::new(memory, capacity).map_err(memory_error)?;
            for item in self.arena.as_slice() {
                check(control, Span::default())?;
                replacement.push(*item).map_err(memory_error)?;
            }
            self.arena = replacement;
        }
        self.arena.push(value).map_err(memory_error)
    }
    pub fn len(&self) -> usize {
        self.arena.len()
    }
    pub fn slice(&self) -> &[T] {
        self.arena.as_slice()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Range {
    pub start: usize,
    pub len: usize,
}
impl Range {
    pub fn get<T>(self, values: &[T]) -> Result<&[T], ParseError> {
        values
            .get(
                self.start
                    ..self
                        .start
                        .checked_add(self.len)
                        .ok_or_else(|| limit(Span::default()))?,
            )
            .ok_or_else(|| invariant(Span::default(), "lowering range"))
    }
}
pub(super) fn region<T>(arena: &QueryArena<'_, '_, T>) -> Result<RetainedRegion, ParseError> {
    RetainedRegion::declared(arena.as_slice().as_ptr() as usize, arena.heap_bytes())
        .map_err(plan_error)
}
pub(super) fn text<'a>(
    bytes: &'a [u8],
    range: Range,
    control: &dyn Fn() -> Result<(), ResourceError>,
) -> Result<&'a str, ParseError> {
    zeppelin_embed::property_graph::checked_utf8(range.get(bytes)?, control).map_err(|error| {
        match error {
            zeppelin_embed::property_graph::Utf8CheckError::Invalid => {
                invariant(Span::default(), "owned UTF-8")
            }
            zeppelin_embed::property_graph::Utf8CheckError::Control(error) => ParseError::new(
                ErrorKind::Resource(error),
                Span::default(),
                "owned UTF-8 control",
            ),
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn copied_text_preserves_four_byte_boundary_and_polls_every_validation_window() {
        let value = format!("{}😀{}", "a".repeat(65535), "λ".repeat(32769));
        let range = Range {
            start: 0,
            len: value.len(),
        };
        let polls = Cell::new(0);
        let control = || {
            polls.set(polls.get() + 1);
            Ok(())
        };
        assert_eq!(text(value.as_bytes(), range, &control).unwrap(), value);
        assert_eq!(polls.get(), 3);
        for fire in 1..=3 {
            polls.set(0);
            let control = || {
                polls.set(polls.get() + 1);
                if polls.get() == fire {
                    Err(ResourceError::ReadCancelled)
                } else {
                    Ok(())
                }
            };
            assert_eq!(
                text(value.as_bytes(), range, &control).unwrap_err().kind,
                ErrorKind::Resource(ResourceError::ReadCancelled)
            );
            assert_eq!(polls.get(), fire);
        }
    }
}
