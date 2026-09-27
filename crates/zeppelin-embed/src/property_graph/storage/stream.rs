//! Bounded retained windows and field cursors over lossless payload extents.
//! A window is not an admission proof: reads resolve its required immutable
//! chunks under the source owner's lease/cache and shared capacity reservation.

use super::payload::{CHUNK_BYTES, PayloadRef};
use super::tree::directory::{BlockSource, NativeReadEvent, TreeError, TreeResources};
use crate::property_graph::{GraphGeneration, StoreInstanceId};
use std::cmp::Ordering;

/// A checked logical window retaining the source that owns its physical bytes.
/// Construction/slicing checks geometry; availability is verified as read.
pub struct PayloadSlice<'a, S: BlockSource> {
    source: &'a S,
    store: StoreInstanceId,
    generation: GraphGeneration,
    payload: PayloadRef,
    offset: u64,
    length: u64,
}
impl<S: BlockSource> Copy for PayloadSlice<'_, S> {}
impl<S: BlockSource> Clone for PayloadSlice<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'a, S: BlockSource> PayloadSlice<'a, S> {
    fn with_span_at<R>(
        self,
        offset: u64,
        maximum: usize,
        resources: &mut TreeResources<'_>,
        callback: impl for<'b, 'r> FnOnce(&'b [u8], &'r mut TreeResources<'_>) -> Result<R, TreeError>,
    ) -> Result<R, TreeError> {
        if offset > self.length {
            return Err(TreeError::Invalid("read beyond payload window"));
        }
        self.payload.with_span_at(
            self.source,
            self.store,
            self.generation,
            self.offset + offset,
            maximum.min((self.length - offset) as usize),
            resources,
            callback,
        )
    }
    /// Retain a complete payload under one store/generation source authority.
    pub fn new(
        source: &'a S,
        store: StoreInstanceId,
        generation: GraphGeneration,
        payload: PayloadRef,
    ) -> Self {
        Self {
            source,
            store,
            generation,
            payload,
            offset: 0,
            length: payload.len(),
        }
    }
    /// Exact logical length, including zero for a present empty field.
    pub const fn len(self) -> u64 {
        self.length
    }
    /// Whether this present field has no bytes.
    pub const fn is_empty(self) -> bool {
        self.length == 0
    }
    /// Original physical payload role; a subfield does not change that role.
    pub const fn role(self) -> super::artifact::BlockKind {
        self.payload.role()
    }
    pub(super) const fn is_whole(self) -> bool {
        self.offset == 0 && self.length == self.payload.len()
    }
    pub(super) fn creation_generation(
        self,
        resources: &mut TreeResources<'_>,
    ) -> Result<GraphGeneration, TreeError> {
        self.payload
            .creation_generation(self.source, self.store, self.generation, resources)
    }
    pub(super) fn linked(
        self,
        payload: PayloadRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        Ok(Self::new(
            self.source,
            self.store,
            self.creation_generation(resources)?,
            payload,
        ))
    }
    /// Exact ordering of two retained windows, independent of physical packing.
    pub fn compare<T: BlockSource>(
        self,
        other: PayloadSlice<'_, T>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Ordering, TreeError> {
        if self.source.scoped_blocks() || other.source.scoped_blocks() {
            let end = self.length.min(other.length);
            let mut position = 0;
            // TreeResources owns a 256 KiB stack envelope for bounded codecs;
            // this one-span copy prevents either mapping from escaping.
            let mut left_copy = [0_u8; CHUNK_BYTES];
            while position < end {
                let left_len = self.with_span_at(
                    position,
                    (end - position) as usize,
                    resources,
                    |bytes, resources| {
                        let count = bytes.len().min((end - position) as usize);
                        let target = left_copy.get_mut(..count).ok_or(TreeError::Memory)?;
                        let source = bytes
                            .get(..count)
                            .ok_or(TreeError::Invalid("left window extent"))?;
                        resources.read_event(NativeReadEvent::CopiedBytes(count as u64))?;
                        target.copy_from_slice(source);
                        Ok(count)
                    },
                )?;
                let (compared, order) =
                    other.with_span_at(position, left_len, resources, |right, resources| {
                        let count = left_len.min(right.len()).min((end - position) as usize);
                        if count == 0 {
                            return Err(TreeError::Invalid("short compared window"));
                        }
                        resources.step(count as u64)?;
                        Ok((
                            count,
                            left_copy
                                .get(..count)
                                .ok_or(TreeError::Invalid("left copied window"))?
                                .cmp(
                                    right
                                        .get(..count)
                                        .ok_or(TreeError::Invalid("right window extent"))?,
                                ),
                        ))
                    })?;
                if order != Ordering::Equal {
                    return Ok(order);
                }
                position += compared as u64;
            }
            resources.step(0)?;
            return Ok(self.length.cmp(&other.length));
        }
        let end = self.length.min(other.length);
        let mut position = 0;
        while position < end {
            let left = self.span_at(position, resources)?;
            let right = other.span_at(position, resources)?;
            let count = left.len().min(right.len()).min((end - position) as usize);
            if count == 0 {
                return Err(TreeError::Invalid("short compared window"));
            }
            resources.step(count as u64)?;
            let order = left
                .get(..count)
                .ok_or(TreeError::Invalid("left window extent"))?
                .cmp(
                    right
                        .get(..count)
                        .ok_or(TreeError::Invalid("right window extent"))?,
                );
            if order != Ordering::Equal {
                return Ok(order);
            }
            position += count as u64;
        }
        resources.step(0)?;
        Ok(self.length.cmp(&other.length))
    }
    /// Retain a bounded subfield; arithmetic cannot escape the original window.
    pub fn subslice(self, offset: u64, length: u64) -> Result<Self, TreeError> {
        if offset
            .checked_add(length)
            .is_none_or(|end| end > self.length)
        {
            return Err(TreeError::Invalid("payload window extent"));
        }
        Ok(Self {
            offset: self
                .offset
                .checked_add(offset)
                .ok_or(TreeError::Invalid("payload window offset"))?,
            length,
            ..self
        })
    }
    /// Borrow the next <=64 KiB physical span, clipped to this exact window.
    pub fn span_at(
        self,
        offset: u64,
        resources: &mut TreeResources<'_>,
    ) -> Result<&'a [u8], TreeError> {
        if offset > self.length {
            return Err(TreeError::Invalid("read beyond payload window"));
        }
        let span = self.payload.span_at_bounded(
            self.source,
            self.store,
            self.generation,
            self.offset + offset,
            (self.length - offset) as usize,
            resources,
        )?;
        span.get(..span.len().min((self.length - offset) as usize))
            .ok_or(TreeError::Invalid("payload window span"))
    }
    /// Copy bounded spans into caller-owned backing without escaping the field.
    pub fn read_at(
        self,
        offset: u64,
        output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        if offset > self.length {
            return Err(TreeError::Invalid("read beyond payload window"));
        }
        let wanted = output.len().min((self.length - offset) as usize);
        let mut copied = 0;
        while copied < wanted {
            let count = self.with_span_at(
                offset + copied as u64,
                wanted - copied,
                resources,
                |bytes, resources| {
                    let count = bytes.len().min(wanted - copied);
                    if count == 0 {
                        return Err(TreeError::Invalid("short payload window"));
                    }
                    let target = output
                        .get_mut(copied..copied + count)
                        .ok_or(TreeError::Memory)?;
                    let source = bytes
                        .get(..count)
                        .ok_or(TreeError::Invalid("payload copy span"))?;
                    resources.step(count as u64)?;
                    resources.read_event(NativeReadEvent::CopiedBytes(count as u64))?;
                    target.copy_from_slice(source);
                    Ok(count)
                },
            )?;
            copied += count;
        }
        resources.step(0)?;
        Ok(copied)
    }
    /// Validate every UTF-8 byte, retaining continuation state across extents.
    /// Empty strings and NUL are legal; this is not a path/name normalization.
    pub fn validate_utf8(self, resources: &mut TreeResources<'_>) -> Result<(), TreeError> {
        let mut state = Utf8State::default();
        let mut position = 0;
        while position < self.length {
            let count = self.with_span_at(
                position,
                (self.length - position) as usize,
                resources,
                |bytes, resources| {
                    if bytes.is_empty() {
                        return Err(TreeError::Invalid("short UTF-8 payload"));
                    }
                    resources.step(bytes.len() as u64)?;
                    state.feed(bytes)?;
                    Ok(bytes.len())
                },
            )?;
            position += count as u64;
        }
        state.finish()?;
        resources.step(0)
    }
    /// Exact byte ordering against an already borrowed name/value, with no copy.
    pub fn compare_bytes(
        self,
        other: &[u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<Ordering, TreeError> {
        let end = self.length.min(other.len() as u64);
        let mut position = 0;
        while position < end {
            let (count, order) = self.with_span_at(
                position,
                (end - position) as usize,
                resources,
                |bytes, resources| {
                    let count = bytes.len().min((end - position) as usize);
                    if count == 0 {
                        return Err(TreeError::Invalid("short compared payload"));
                    }
                    resources.step(count as u64)?;
                    let order = bytes
                        .get(..count)
                        .ok_or(TreeError::Invalid("compared payload span"))?
                        .cmp(
                            other
                                .get(position as usize..position as usize + count)
                                .ok_or(TreeError::Invalid("compared input span"))?,
                        );
                    Ok((count, order))
                },
            )?;
            if order != Ordering::Equal {
                return Ok(order);
            }
            position += count as u64;
        }
        resources.step(0)?;
        Ok(self.length.cmp(&(other.len() as u64)))
    }
}

/// Sequential fixed-field decoder with one borrowed span cache. Reading a byte
/// does not re-resolve/re-checksum its entire chunk. The caller owns this fixed
/// stack descriptor inside its operation reservation; no heap is allocated.
pub struct PayloadCursor<'a, 'm, S: BlockSource> {
    source: PayloadSlice<'a, S>,
    position: u64,
    cache_start: u64,
    cache: &'a [u8],
    copied: Option<super::tree::directory::TreeReadBuffer<'m>>,
    copied_len: usize,
}
impl<'a, 'm, S: BlockSource> PayloadCursor<'a, 'm, S> {
    /// Start at the first byte of one bounded retained payload window.
    pub fn new(source: PayloadSlice<'a, S>) -> Self {
        Self {
            source,
            position: 0,
            cache_start: 0,
            cache: &[],
            copied: None,
            copied_len: 0,
        }
    }
    /// Start a cursor that lazily allocates one charged 64 KiB copied-span
    /// cache only when its authenticated source requires scoped reads.
    pub fn new_with_resources(
        source: PayloadSlice<'a, S>,
        resources: &mut TreeResources<'m>,
    ) -> Result<Self, TreeError> {
        let copied = if source.source.scoped_blocks() {
            Some(resources.copied_span_buffer(CHUNK_BYTES)?)
        } else {
            None
        };
        Ok(Self {
            source,
            position: 0,
            cache_start: 0,
            cache: &[],
            copied,
            copied_len: 0,
        })
    }
    /// Logical bytes consumed, relative to this cursor's original window.
    pub const fn position(&self) -> u64 {
        self.position
    }
    /// Decode one fixed-width field, bounded to 64 bytes even for generic callers.
    pub fn read_array<const N: usize>(
        &mut self,
        resources: &mut TreeResources<'_>,
    ) -> Result<[u8; N], TreeError> {
        resources.step(1)?;
        if N > 64
            || self
                .position
                .checked_add(N as u64)
                .is_none_or(|end| end > self.source.length)
        {
            return Err(TreeError::Invalid("fixed field extent"));
        }
        let mut output = [0; N];
        let mut copied = 0;
        while copied < N {
            if let Some(cache) = self.copied.as_mut() {
                let cache_end = self.cache_start + self.copied_len as u64;
                if self.position < self.cache_start || self.position >= cache_end {
                    self.copied_len =
                        self.source
                            .read_at(self.position, cache.as_mut_slice(), resources)?;
                    self.cache_start = self.position;
                }
                let start = (self.position - self.cache_start) as usize;
                let count = self.copied_len.saturating_sub(start).min(N - copied);
                if count == 0 {
                    return Err(TreeError::Invalid("short fixed field"));
                }
                let target = output
                    .get_mut(copied..copied + count)
                    .ok_or(TreeError::Memory)?;
                let source = cache
                    .as_slice()
                    .get(start..start + count)
                    .ok_or(TreeError::Invalid("copied field span"))?;
                copy_cached_field(target, source, resources)?;
                copied += count;
                self.position += count as u64;
                continue;
            }
            let cache_end = self.cache_start + self.cache.len() as u64;
            if self.position < self.cache_start || self.position >= cache_end {
                self.cache = self.source.span_at(self.position, resources)?;
                self.cache_start = self.position;
            }
            let start = (self.position - self.cache_start) as usize;
            let count = self.cache.len().saturating_sub(start).min(N - copied);
            if count == 0 {
                return Err(TreeError::Invalid("short fixed field"));
            }
            let target = output
                .get_mut(copied..copied + count)
                .ok_or(TreeError::Memory)?;
            let source = self
                .cache
                .get(start..start + count)
                .ok_or(TreeError::Invalid("cached field span"))?;
            resources.step(count as u64)?;
            resources.read_event(NativeReadEvent::CopiedBytes(count as u64))?;
            target.copy_from_slice(source);
            copied += count;
            self.position += count as u64;
        }
        resources.step(0)?;
        Ok(output)
    }
    /// Retain the next exact field; its bytes are validated by its role decoder.
    pub fn take(
        &mut self,
        length: u64,
        resources: &mut TreeResources<'_>,
    ) -> Result<PayloadSlice<'a, S>, TreeError> {
        resources.step(1)?;
        let field = self.source.subslice(self.position, length)?;
        self.position += length;
        Ok(field)
    }
    /// Decode a canonical u64-length-prefixed field without allocating/copying it.
    pub fn blob(
        &mut self,
        resources: &mut TreeResources<'_>,
    ) -> Result<PayloadSlice<'a, S>, TreeError> {
        let length = u64::from_le_bytes(self.read_array(resources)?);
        self.take(length, resources)
    }
    /// Reject trailing bytes and poll after the last validated field.
    pub fn finish(self, resources: &mut TreeResources<'_>) -> Result<(), TreeError> {
        resources.step(0)?;
        if self.position != self.source.length {
            return Err(TreeError::Invalid("trailing payload fields"));
        }
        Ok(())
    }
}

/// Shared streaming recognizer for canonical names, text and overflow keys.
pub(super) struct Utf8State {
    remaining: u8,
    low: u8,
    high: u8,
}
impl Default for Utf8State {
    fn default() -> Self {
        Self {
            remaining: 0,
            low: 0x80,
            high: 0xbf,
        }
    }
}
impl Utf8State {
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<(), TreeError> {
        if bytes.len() > CHUNK_BYTES {
            return Err(TreeError::Invalid("UTF-8 span bound"));
        }
        for byte in bytes {
            if self.remaining != 0 {
                if *byte < self.low || *byte > self.high {
                    return Err(TreeError::Invalid("invalid UTF-8 continuation"));
                }
                self.remaining -= 1;
                self.low = 0x80;
                self.high = 0xbf;
            } else {
                match *byte {
                    0..=0x7f => {}
                    0xc2..=0xdf => self.remaining = 1,
                    0xe0 => {
                        self.remaining = 2;
                        self.low = 0xa0;
                    }
                    0xe1..=0xec | 0xee..=0xef => self.remaining = 2,
                    0xed => {
                        self.remaining = 2;
                        self.high = 0x9f;
                    }
                    0xf0 => {
                        self.remaining = 3;
                        self.low = 0x90;
                    }
                    0xf1..=0xf3 => self.remaining = 3,
                    0xf4 => {
                        self.remaining = 3;
                        self.high = 0x8f;
                    }
                    _ => return Err(TreeError::Invalid("invalid UTF-8 lead")),
                }
            }
        }
        Ok(())
    }
    pub(super) fn finish(self) -> Result<(), TreeError> {
        if self.remaining != 0 {
            return Err(TreeError::Invalid("truncated UTF-8"));
        }
        Ok(())
    }
}

// Separate the copy so tests can inspect its destination on refusal; read_array
// discards its local array when it returns an error.
fn copy_cached_field(
    target: &mut [u8],
    source: &[u8],
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    resources.step(source.len() as u64)?;
    resources.read_event(NativeReadEvent::CopiedBytes(source.len() as u64))?;
    target.copy_from_slice(source);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
    use crate::property_graph::query::resources::QueryMemory;
    use crate::property_graph::query::runtime::{
        RetainedView, RuntimeContext, RuntimeError, RuntimeLimits, WorkKind,
    };
    use crate::property_graph::query::{QueryError, QueryView};
    use crate::property_graph::resources::GraphResources;

    struct View(QueryView);
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.0
        }
        fn check_active(&self) -> Result<(), QueryError> {
            Ok(())
        }
    }

    #[test]
    fn cached_field_copy_refusal_preserves_destination() {
        let directory = tempfile::tempdir().expect("directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
        )
        .expect("store");
        let shared = GraphResources::from_store(&store).expect("accounting");
        let memory = QueryMemory::new(&shared, 512 * 1024).expect("memory");
        let view = View(QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        ));
        let control = QueryControl::Cancel(CancelToken::new());
        let limits = RuntimeLimits::default()
            .with_limit(WorkKind::CopiedBytes, 0)
            .expect("limit");
        let mut context = RuntimeContext::new(&view, &control, &memory, limits).expect("runtime");
        {
            let mut resources = TreeResources::for_query(&mut context).expect("resources");
            let mut output = [0xaa];
            assert!(matches!(
                copy_cached_field(&mut output, b"p", &mut resources),
                Err(TreeError::Runtime(RuntimeError::Limit(
                    WorkKind::CopiedBytes
                )))
            ));
            assert_eq!(output, [0xaa]);
        }
        assert_eq!(context.counters().get(WorkKind::CopiedBytes), 0);
    }
}
