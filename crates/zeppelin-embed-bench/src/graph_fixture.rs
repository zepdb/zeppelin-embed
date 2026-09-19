//! Versioned native graph fixture generation. These are tooling keys, never
//! caller-selected engine IDs. Product ingestion and timing are separate tools.
use std::collections::BTreeMap;

pub const VERSION: &str = "graph-fixture-v1";
pub const ROOT_SEED: u64 = 0x4752_4150_4830_3031;
pub const DIMS: usize = 768;
pub const CENTROIDS: usize = 64;
pub type Error = String;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scale {
    Baseline,
    Stress,
    Small,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    pub scale: Scale,
    pub seed: u64,
}
impl Config {
    pub fn new(scale: Scale) -> Self {
        Self {
            scale,
            seed: ROOT_SEED,
        }
    }
    pub fn meetings(self) -> u64 {
        match self.scale {
            Scale::Baseline => 9125,
            Scale::Stress => 91250,
            Scale::Small => 4,
        }
    }
    pub fn people(self) -> u64 {
        match self.scale {
            Scale::Baseline => 10000,
            Scale::Stress => 100000,
            Scale::Small => 12,
        }
    }
    pub fn projects(self) -> u64 {
        match self.scale {
            Scale::Baseline => 2000,
            Scale::Stress => 20000,
            Scale::Small => 3,
        }
    }
    pub fn topics(self) -> u64 {
        match self.scale {
            Scale::Baseline => 8000,
            Scale::Stress => 80000,
            Scale::Small => 8,
        }
    }
    pub fn shared(self) -> u64 {
        self.people() + self.projects() + self.topics()
    }
    pub fn node_count(self) -> u64 {
        self.shared() + 27 * self.meetings()
    }
    pub fn edge_count(self) -> u64 {
        110 * self.meetings()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Kind {
    Person,
    Project,
    Topic,
    Meeting,
    Chunk,
    Decision,
    Action,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct NodeKey {
    pub kind: Kind,
    pub index: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeRecord {
    pub ordinal: u64,
    pub key: NodeKey,
    pub topic: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdgeRecord {
    pub ordinal: u64,
    pub source: NodeKey,
    pub target: NodeKey,
    pub kind: &'static str,
}
#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    pub nodes: u64,
    pub edges: u64,
    pub vectors: u64,
    pub vector_bytes: u64,
    pub absent_text: u64,
    pub empty_text: u64,
    pub whitespace_text: u64,
    pub indexed_text: u64,
    pub both: u64,
    pub vector_only: u64,
    pub text_only: u64,
    pub neither: u64,
    pub node_kinds: BTreeMap<String, u64>,
    pub edge_kinds: BTreeMap<String, u64>,
    pub indegree: BTreeMap<u64, u64>,
    pub outdegree: BTreeMap<u64, u64>,
    pub degree: BTreeMap<u64, u64>,
    pub project_hub_degree: u64,
    pub max_batch_changes: usize,
    pub batches: u64,
}
mod topology;
pub use topology::{inventory, node_key, ordinal, participants, project, visit};
impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Project => "project",
            Self::Topic => "topic",
            Self::Meeting => "meeting",
            Self::Chunk => "chunk",
            Self::Decision => "decision",
            Self::Action => "action",
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Person => "Person",
            Self::Project => "Project",
            Self::Topic => "Topic",
            Self::Meeting => "Meeting",
            Self::Chunk => "Chunk",
            Self::Decision => "Decision",
            Self::Action => "Action",
        }
    }
}
impl NodeKey {
    pub fn namespace(self) -> String {
        format!("fixture-v1/{}", self.kind.name())
    }
}

mod vectors;
pub use vectors::{VectorGenerator, WordStream};

mod files;
pub use files::{
    FileManifest, FixtureState, read_manifest, validate_fixture, visit_batches, write_fixture,
};
