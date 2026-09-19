// Syntax decomposition adapted from Shopify/cypher-parser at
// a7b822fbece9ee2c3f2b57ecfd4e90a2ca215383, src/ast.rs.
// Copyright (c) 2025-present Shopify Inc. See ../SHOPIFY-LICENSE-MIT.
use crate::{ParseError, Resources, Span, lexer::Token, resources::poll};

/// Syntax arena identity; it is never a core plan or graph identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AstId(pub(crate) usize);
/// Lexical name/string identity; distinct from syntax and core IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextId(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
    Both,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathBounds {
    pub lower: u32,
    pub upper: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    Not,
    IsNull,
    IsNotNull,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    Xor,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    StartsWith,
    EndsWith,
    Contains,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Function {
    Count,
    Collect,
    Labels,
    Type,
    Size,
    NodeId,
    RelationshipId,
    StoredText,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Procedure {
    VectorSearch,
    TextSearch,
    HybridSearch,
}

/// Children are source ordered arena IDs, never recursively owned syntax trees.
/// Pattern children alternate node/relationship. Projection children are items,
/// followed by order items, SKIP, LIMIT and (WITH only) predicate. Names in pattern
/// children are conjunctive labels or alternative relationship types.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NodeKind {
    Statement,
    Match {
        optional: bool,
    },
    Create,
    Set,
    Remove,
    Delete {
        detach: bool,
    },
    Projection {
        with: bool,
        distinct: bool,
    },
    Call(Procedure),
    Pattern,
    NodePattern {
        variable: Option<TextId>,
    },
    RelationshipPattern {
        variable: Option<TextId>,
        direction: Direction,
        bounds: Option<PathBounds>,
    },
    Name(TextId),
    Properties,
    Property(TextId),
    Predicate,
    ProjectionItem {
        alias: Option<TextId>,
    },
    Star,
    Order {
        descending: bool,
    },
    Skip,
    Limit,
    Yield {
        name: TextId,
        alias: Option<TextId>,
    },
    SetProperty {
        variable: TextId,
        property: TextId,
    },
    SetLabels {
        variable: TextId,
    },
    RemoveProperty {
        variable: TextId,
        property: TextId,
    },
    RemoveLabels {
        variable: TextId,
    },
    Variable(TextId),
    Parameter(TextId),
    Integer(i64),
    Float(f64),
    String(TextId),
    Boolean(bool),
    Null,
    List,
    Group,
    PropertyAccess(TextId),
    LabelPredicate,
    Index,
    Unary(UnaryOp),
    Binary(BinaryOp),
    Function {
        function: Function,
        distinct: bool,
    },
}

#[derive(Debug, PartialEq)]
pub struct Node {
    pub kind: NodeKind,
    pub span: Span,
    pub(crate) children: Vec<AstId>,
    pub(crate) aggregate: bool,
}
impl Node {
    pub fn children(&self) -> &[AstId] {
        &self.children
    }
}

/// Complete syntax only. No store handle, catalog IDs, writer or partially
/// executable prefix can be obtained from this value.
#[derive(Debug)]
pub struct Ast {
    pub(crate) source: String,
    pub(crate) tokens: Vec<Token>,
    pub(crate) nodes: Vec<Node>,
    pub(crate) root: AstId,
}
impl Ast {
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn root(&self) -> AstId {
        self.root
    }
    pub fn node(&self, id: AstId) -> Option<&Node> {
        self.nodes.get(id.0)
    }
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
    pub fn text(&self, id: TextId) -> Option<&str> {
        self.tokens
            .get(id.0)
            .and_then(|token| token.text(&self.source))
    }
    /// A bounded iterative postorder walk. Shared comparison operands are visited
    /// once. A consumer can use child IDs to reuse their computed values.
    pub fn visit(
        &self,
        resources: &mut dyn Resources,
        mut visitor: impl FnMut(AstId, &Node) -> Result<(), ParseError>,
    ) -> Result<(), ParseError> {
        for (index, node) in self.nodes.iter().enumerate() {
            poll(resources, node.span)?;
            visitor(AstId(index), node)?;
        }
        Ok(())
    }
}
