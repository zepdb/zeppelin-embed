use super::super::canonical::{Encoder, encode_document, encode_staging_value};
use super::*;
use memory::Arena;
use std::io::{self, Write};

pub(super) struct Encoded<'a> {
    pub(super) bytes: Arena<'a, u8>,
    pub(super) shape: EntityShape<'a>,
    pub(super) fingerprint: CanonicalFingerprint,
    pub(super) membership: Membership,
}
pub(super) struct PreparedImage<'a> {
    relationship_type: Option<GraphName<'a>>,
    labels: &'a [GraphName<'a>],
    node_properties: &'a [GraphProperty<'a>],
    sorted: Arena<'a, GraphProperty<'a>>,
    text: Option<&'a str>,
    embedding: Option<CanonicalEmbedding<'a>>,
    label_count: usize,
    length: usize,
    memory: &'a WriteMemory<'a>,
}
impl<'a> PreparedImage<'a> {
    pub(super) fn new(
        input: WriteImage<'a, '_>,
        base: &dyn AdmittedBase,
        memory: &'a WriteMemory<'a>,
        control: &mut WriteControl<'_>,
    ) -> Result<Self, StageError> {
        if let WriteImage::Relationship { properties, .. } = input
            && properties.len() > memory.limits.input_bytes / 8
        {
            return Err(StageError::Limit);
        }
        let mut sorted = Arena::new(
            memory,
            match input {
                WriteImage::Relationship { properties, .. } => properties.len(),
                _ => 0,
            },
            control,
        )?;
        let (relationship_type, labels, node_properties, text, embedding) = match input {
            WriteImage::Node(image) => {
                let (labels, properties, text, embedding) =
                    image.staging_node_parts().ok_or(StageError::InvalidInput)?;
                (None, labels, properties, text, embedding)
            }
            WriteImage::Relationship {
                relationship_type,
                properties,
                ..
            } => {
                if properties.len() > memory.limits.input_bytes / 8 {
                    return Err(StageError::Limit);
                }
                for property in properties {
                    control(WritePhase::Validate)?;
                    sorted.push(*property)?;
                }
                bounded::sort(sorted.as_mut_slice(), control, |a, b, c| {
                    bounded::bytes(
                        a.name().as_str().as_bytes(),
                        b.name().as_str().as_bytes(),
                        c,
                    )
                })?;
                let mut previous = None;
                for property in &*sorted {
                    control(WritePhase::Validate)?;
                    if let Some(old) = previous
                        && bounded::bytes(old, property.name().as_str().as_bytes(), control)?
                            .is_eq()
                    {
                        return Err(CanonicalError::DuplicateProperty.into());
                    }
                    previous = Some(property.name().as_str().as_bytes());
                }
                (Some(relationship_type), &[][..], &[][..], None, None)
            }
        };
        base.interpretation().validate_payload(embedding, &mut || {
            control(WritePhase::Validate).map_err(|_| catalog::CatalogError::Cancelled)
        })?;
        let mut count = 0usize;
        let mut previous = None;
        for label in labels {
            control(WritePhase::Validate)?;
            let distinct = match previous {
                None => true,
                Some(old) => !bounded::bytes(old, label.as_str().as_bytes(), control)?.is_eq(),
            };
            if distinct {
                count = count.checked_add(1).ok_or(StageError::Limit)?;
            }
            previous = Some(label.as_str().as_bytes());
        }
        let mut prepared = Self {
            relationship_type,
            labels,
            node_properties,
            sorted,
            text,
            embedding,
            label_count: count,
            length: 0,
            memory,
        };
        prepared.length = prepared.write_to(&mut io::sink(), None, control)?;
        if prepared.length > memory.limits.input_bytes {
            return Err(StageError::Limit);
        }
        Ok(prepared)
    }
    pub(super) const fn len(&self) -> usize {
        self.length
    }
    fn write_to(
        &self,
        output: &mut dyn Write,
        endpoints: Option<(NodeId, NodeId)>,
        control: &mut WriteControl<'_>,
    ) -> Result<usize, StageError> {
        let mut poll = || control(WritePhase::Canonical).map_err(|_| CanonicalError::Cancelled);
        let mut e = Encoder::new(output, &mut poll);
        e.emit(b"ZGCI")?;
        e.emit(&1u16.to_le_bytes())?;
        match self.relationship_type {
            None => {
                e.byte(1)?;
                e.count(self.label_count)?;
                // Sorted labels are emitted once. Comparison is chunk-polled
                // before the encoder borrow below via the callback adapter.
                let mut previous: Option<GraphName<'_>> = None;
                for label in self.labels {
                    let distinct = match previous {
                        None => true,
                        Some(old) => {
                            let mut equal = old.as_str().len() == label.as_str().len();
                            for (a, b) in old
                                .as_str()
                                .as_bytes()
                                .chunks(65536)
                                .zip(label.as_str().as_bytes().chunks(65536))
                            {
                                e.staging_checkpoint()?;
                                if a != b {
                                    equal = false;
                                    break;
                                }
                            }
                            !equal
                        }
                    };
                    if distinct {
                        e.blob(label.as_str().as_bytes())?;
                    }
                    previous = Some(*label);
                }
            }
            Some(relationship_type) => {
                e.byte(2)?;
                if let Some((source, target)) = endpoints {
                    e.emit(&source.get().to_le_bytes())?;
                    e.emit(&target.get().to_le_bytes())?;
                } else {
                    // Only new() passes unresolved endpoints, to io::sink.
                    e.emit(&[0u8; 32])?;
                }
                e.blob(relationship_type.as_str().as_bytes())?;
            }
        }
        let properties = if self.relationship_type.is_some() {
            &*self.sorted
        } else {
            self.node_properties
        };
        e.count(properties.len())?;
        for property in properties {
            e.blob(property.name().as_str().as_bytes())?;
            encode_staging_value(&mut e, property.value().data())?;
        }
        e.optional_blob(self.text.map(str::as_bytes))?;
        e.byte(u8::from(self.embedding.is_some()))?;
        if let Some(embedding) = self.embedding {
            encode_document(&mut e, embedding.document())?;
            e.count(embedding.vector().coordinates().len())?;
            for value in embedding.vector().coordinates() {
                e.emit(&value.to_bits().to_le_bytes())?;
            }
        }
        Ok(e.stats().bytes as usize)
    }
    pub(super) fn encode(
        self,
        endpoints: Option<(NodeId, NodeId)>,
        control: &mut WriteControl<'_>,
    ) -> Result<Encoded<'a>, StageError> {
        let _work = self.memory.resources().begin_work();
        let shape = match self.relationship_type {
            None => EntityShape::Node,
            Some(relationship_type) => {
                let (source, target) = endpoints.ok_or(StageError::Endpoint)?;
                EntityShape::Relationship {
                    source,
                    target,
                    relationship_type,
                }
            }
        };
        let mut bytes = Arena::new(self.memory, self.length, control)?;
        let mut output = Hashed {
            output: &mut bytes,
            resources: self.memory.resources(),
            hash: xxhash_rust::xxh3::Xxh3::new(),
        };
        self.write_to(&mut output, endpoints, control)?;
        let fingerprint = CanonicalFingerprint::new(self.length as u64, output.hash.digest())?;
        Ok(Encoded {
            bytes,
            shape,
            fingerprint,
            membership: Membership {
                text: self.text.is_some(),
                vector: self.embedding.is_some(),
            },
        })
    }
}
struct Hashed<'a> {
    output: &'a mut dyn Write,
    resources: &'a super::super::resources::GraphResources,
    hash: xxhash_rust::xxh3::Xxh3,
}
impl Write for Hashed<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.output.write(bytes)?;
        self.resources.record_work(
            crate::lifecycle::stats::GraphWorkKind::CanonicalEncodingBytes,
            count as u64,
        );
        self.hash
            .update(bytes.get(..count).ok_or(io::ErrorKind::InvalidData)?);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
