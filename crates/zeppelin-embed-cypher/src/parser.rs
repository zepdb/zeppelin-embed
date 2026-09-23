// Complete-input admission and small clause/pattern/projection descent adapted
// from Shopify/cypher-parser a7b822fbece9ee2c3f2b57ecfd4e90a2ca215383,
// src/parser.rs. Copyright (c) 2025-present Shopify Inc.; ../SHOPIFY-LICENSE-MIT.
// Local Pratt expressions, profile, flat arenas, spans and resource accounting.
use crate::lexer::{Kind, Token};
use crate::resources::{poll, push};
use crate::*;

pub(crate) fn parse(
    source: String,
    tokens: Vec<Token>,
    limits: CompileLimits,
    resources: &mut dyn Resources,
) -> Result<Ast, ParseError> {
    let mut parser = Parser {
        ast: Ast {
            source,
            tokens,
            nodes: Vec::new(),
            root: AstId(0),
        },
        position: 0,
        limits,
        resources,
        depth: 0,
        list_depth: 0,
        parameters: Vec::new(),
    };
    let root = parser.statement()?;
    parser.ast.root = root;
    Ok(parser.ast)
}
struct Parser<'a> {
    ast: Ast,
    position: usize,
    limits: CompileLimits,
    resources: &'a mut dyn Resources,
    depth: usize,
    list_depth: usize,
    parameters: Vec<TextId>,
}
impl Parser<'_> {
    fn span(&self) -> Span {
        self.ast.tokens.get(self.position).map_or(
            Span {
                start: self.ast.source.len(),
                end: self.ast.source.len(),
            },
            |token| token.span,
        )
    }
    fn end(&self) -> usize {
        self.position
            .checked_sub(1)
            .and_then(|index| self.ast.tokens.get(index))
            .map_or(0, |token| token.span.end)
    }
    fn kind(&self) -> Option<Kind> {
        self.ast.tokens.get(self.position).map(|token| token.kind)
    }
    fn peek_kind(&self, offset: usize) -> Option<Kind> {
        self.ast
            .tokens
            .get(self.position + offset)
            .map(|token| token.kind)
    }
    fn word(&self, word: &str) -> bool {
        self.kind() == Some(Kind::Ident)
            && self
                .ast
                .tokens
                .get(self.position)
                .and_then(|t| t.text(&self.ast.source))
                .is_some_and(|text| text.eq_ignore_ascii_case(word))
    }
    fn advance(&mut self) -> Result<TextId, ParseError> {
        let span = self.span();
        poll(self.resources, span)?;
        let id = TextId(self.position);
        if self.kind().is_none() {
            return Err(self.error(ErrorKind::Syntax, "unexpected end of input"));
        }
        self.position += 1;
        Ok(id)
    }
    fn eat(&mut self, kind: Kind) -> Result<bool, ParseError> {
        if self.kind() == Some(kind) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn eat_word(&mut self, word: &str) -> Result<bool, ParseError> {
        if self.word(word) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn expect(&mut self, kind: Kind, message: &'static str) -> Result<(), ParseError> {
        if kind == Kind::RBracket && matches!(self.kind(), Some(Kind::DotDot | Kind::Pipe)) {
            return Err(self.error(
                ErrorKind::Unsupported,
                "list slices and comprehensions are outside profile",
            ));
        }
        if self.eat(kind)? {
            Ok(())
        } else {
            Err(self.error(ErrorKind::Syntax, message))
        }
    }
    fn expect_word(&mut self, word: &str) -> Result<(), ParseError> {
        if self.eat_word(word)? {
            Ok(())
        } else {
            Err(self.error(ErrorKind::Syntax, "expected keyword"))
        }
    }
    fn error(&self, kind: ErrorKind, message: &'static str) -> ParseError {
        ParseError::new(kind, self.span(), message)
    }
    fn name_error(&self, id: TextId, message: &'static str) -> ParseError {
        ParseError::new(
            ErrorKind::Unsupported,
            self.ast
                .tokens
                .get(id.0)
                .map_or(self.span(), |token| token.span),
            message,
        )
    }
    fn text(&self, id: TextId) -> Result<&str, ParseError> {
        self.ast
            .text(id)
            .ok_or_else(|| self.error(ErrorKind::Syntax, "invalid lexical ID"))
    }
    fn variable_name(&mut self) -> Result<TextId, ParseError> {
        if self.kind() == Some(Kind::Ident)
            && (self.reserved() || self.word("NULL") || self.word("TRUE") || self.word("FALSE"))
        {
            return Err(self.error(
                ErrorKind::Syntax,
                "reserved variable or alias requires backticks",
            ));
        }
        self.name()
    }
    fn name(&mut self) -> Result<TextId, ParseError> {
        if matches!(self.kind(), Some(Kind::Parameter | Kind::Star)) {
            return Err(self.error(
                ErrorKind::Unsupported,
                "dynamic names and YIELD wildcard are outside profile",
            ));
        }
        if matches!(self.kind(), Some(Kind::Ident | Kind::QuotedName)) {
            self.advance()
        } else {
            Err(self.error(ErrorKind::Syntax, "expected name"))
        }
    }
    fn add(
        &mut self,
        kind: NodeKind,
        start: usize,
        children: Vec<AstId>,
    ) -> Result<AstId, ParseError> {
        let span = Span {
            start,
            end: self.end(),
        };
        poll(self.resources, span)?;
        if self.ast.nodes.len() == self.limits.ast_nodes {
            return Err(ParseError::new(
                ErrorKind::Limit(LimitKind::AstNodes),
                span,
                "AST node limit",
            ));
        }
        let id = AstId(self.ast.nodes.len());
        let aggregate = matches!(
            kind,
            NodeKind::Function {
                function: Function::Count | Function::Collect,
                ..
            }
        ) || children
            .iter()
            .any(|id| self.ast.node(*id).is_some_and(|node| node.aggregate));
        push(
            &mut self.ast.nodes,
            Node {
                kind,
                span,
                children,
                aggregate,
            },
            self.resources,
            span,
        )?;
        Ok(id)
    }
    fn children(&mut self, ids: &[AstId]) -> Result<Vec<AstId>, ParseError> {
        let mut result = Vec::new();
        for id in ids {
            let span = self.span();
            push(&mut result, *id, self.resources, span)?;
        }
        Ok(result)
    }
    fn leaf(&mut self, kind: NodeKind, start: usize) -> Result<AstId, ParseError> {
        self.add(kind, start, Vec::new())
    }
    fn wrap(&mut self, kind: NodeKind, start: usize, ids: &[AstId]) -> Result<AstId, ParseError> {
        let children = self.children(ids)?;
        self.add(kind, start, children)
    }
    fn node(&self, id: AstId) -> Result<&Node, ParseError> {
        self.ast
            .node(id)
            .ok_or_else(|| self.error(ErrorKind::Syntax, "invalid AST ID"))
    }
    fn append(&mut self, children: &mut Vec<AstId>, id: AstId) -> Result<(), ParseError> {
        let span = self.span();
        push(children, id, self.resources, span)
    }

    fn statement(&mut self) -> Result<AstId, ParseError> {
        let start = self.span().start;
        let mut clauses = Vec::new();
        let mut mutated = false;
        let mut called = false;
        let mut returned = false;
        loop {
            let clause_start = self.span().start;
            let clause = if self.word("MATCH") || self.word("OPTIONAL") {
                if mutated {
                    return Err(self.error(ErrorKind::Unsupported, "reading clause after update"));
                }
                let optional = self.eat_word("OPTIONAL")?;
                self.expect_word("MATCH")?;
                let mut patterns = self.patterns(false)?;
                self.where_clause(&mut patterns)?;
                self.add(NodeKind::Match { optional }, clause_start, patterns)?
            } else if self.eat_word("CREATE")? {
                if called {
                    return Err(self.error(ErrorKind::Unsupported, "search in mutating statement"));
                }
                mutated = true;
                let patterns = self.patterns(true)?;
                self.add(NodeKind::Create, clause_start, patterns)?
            } else if self.word("SET") || self.word("REMOVE") {
                if called {
                    return Err(self.error(ErrorKind::Unsupported, "search in mutating statement"));
                }
                mutated = true;
                let remove = self.eat_word("REMOVE")?;
                if !remove {
                    self.expect_word("SET")?;
                }
                self.update_items(remove, clause_start)?
            } else if self.word("DELETE") || self.word("DETACH") {
                if called {
                    return Err(self.error(ErrorKind::Unsupported, "search in mutating statement"));
                }
                mutated = true;
                let detach = self.eat_word("DETACH")?;
                self.expect_word("DELETE")?;
                let mut variables = Vec::new();
                loop {
                    let variable_start = self.span().start;
                    if matches!(
                        self.kind(),
                        Some(Kind::LBracket | Kind::LParen | Kind::LBrace)
                    ) {
                        return Err(
                            self.error(ErrorKind::Unsupported, "DELETE requires entity variables")
                        );
                    }
                    let name = self.variable_name()?;
                    let variable = self.leaf(NodeKind::Variable(name), variable_start)?;
                    self.append(&mut variables, variable)?;
                    if !self.eat(Kind::Comma)? {
                        break;
                    }
                }
                self.add(NodeKind::Delete { detach }, clause_start, variables)?
            } else if self.eat_word("CALL")? {
                if mutated {
                    return Err(self.error(ErrorKind::Unsupported, "search in mutating statement"));
                }
                called = true;
                self.call(clause_start)?
            } else if self.eat_word("WITH")? {
                self.projection(true)?
            } else if self.eat_word("RETURN")? {
                returned = true;
                self.projection(false)?
            } else {
                break;
            };
            self.append(&mut clauses, clause)?;
            if returned {
                break;
            }
        }
        if [
            "UNION", "USE", "START", "LOAD", "BEGIN", "COMMIT", "ROLLBACK", "EXPLAIN", "PROFILE",
            "MERGE", "UNWIND", "FOREACH", "DROP", "ALTER",
        ]
        .iter()
        .any(|word| self.word(word))
        {
            return Err(self.error(ErrorKind::Unsupported, "statement form outside profile"));
        }
        if clauses.is_empty() || (!returned && !mutated) {
            return Err(self.error(ErrorKind::Syntax, "read query must end with RETURN"));
        }
        if !returned
            && clauses
                .last()
                .and_then(|id| self.ast.node(*id))
                .is_some_and(|n| matches!(n.kind, NodeKind::Projection { .. }))
        {
            return Err(self.error(ErrorKind::Syntax, "query cannot end with WITH"));
        }
        self.eat(Kind::Semicolon)?;
        if self.kind().is_some() {
            return Err(self.error(
                ErrorKind::Unsupported,
                "unsupported or trailing statement input",
            ));
        }
        self.add(NodeKind::Statement, start, clauses)
    }
    fn where_clause(&mut self, items: &mut Vec<AstId>) -> Result<(), ParseError> {
        let start = self.span().start;
        if self.eat_word("WHERE")? {
            let expression = self.expression(0)?;
            self.check_aggregate(expression, false)?;
            let predicate = self.wrap(NodeKind::Predicate, start, &[expression])?;
            self.append(items, predicate)?;
        }
        Ok(())
    }
    fn patterns(&mut self, create: bool) -> Result<Vec<AstId>, ParseError> {
        let mut patterns = Vec::new();
        loop {
            let start = self.span().start;
            if self.kind() != Some(Kind::LParen) {
                return Err(self.error(
                    ErrorKind::Unsupported,
                    "only unnamed node/relationship patterns are supported",
                ));
            }
            let node = self.node_pattern()?;
            let mut parts = self.children(&[node])?;
            while matches!(self.kind(), Some(Kind::Minus | Kind::Lt)) {
                let relationship = self.relationship(create)?;
                self.append(&mut parts, relationship)?;
                let node = self.node_pattern()?;
                self.append(&mut parts, node)?;
            }
            let pattern = self.add(NodeKind::Pattern, start, parts)?;
            self.append(&mut patterns, pattern)?;
            if !self.eat(Kind::Comma)? {
                break;
            }
        }
        Ok(patterns)
    }
    fn labels(&mut self, items: &mut Vec<AstId>) -> Result<(), ParseError> {
        while self.eat(Kind::Colon)? {
            let start = self.span().start;
            let name = self.name()?;
            let label = self.leaf(NodeKind::Name(name), start)?;
            self.append(items, label)?;
        }
        Ok(())
    }
    fn node_pattern(&mut self) -> Result<AstId, ParseError> {
        let start = self.span().start;
        self.expect(Kind::LParen, "expected node pattern")?;
        let variable = if matches!(self.kind(), Some(Kind::Ident | Kind::QuotedName)) {
            Some(self.variable_name()?)
        } else {
            None
        };
        let mut items = Vec::new();
        self.labels(&mut items)?;
        if self.kind() == Some(Kind::LBrace) {
            let props = self.properties()?;
            self.append(&mut items, props)?;
        }
        if self.kind() == Some(Kind::Parameter) {
            return Err(self.error(
                ErrorKind::InvalidParameterUse,
                "whole-map pattern parameter",
            ));
        }
        self.expect(Kind::RParen, "expected closing node parenthesis")?;
        self.add(NodeKind::NodePattern { variable }, start, items)
    }
    fn properties(&mut self) -> Result<AstId, ParseError> {
        let start = self.span().start;
        self.expect(Kind::LBrace, "expected property bag")?;
        let mut properties = Vec::new();
        if !self.eat(Kind::RBrace)? {
            loop {
                let span = self.span();
                let key = self.name()?;
                for prior in &properties {
                    poll(self.resources, span)?;
                    if let NodeKind::Property(name) = self.node(*prior)?.kind
                        && self.text(key)? == self.text(name)?
                    {
                        return Err(ParseError::new(
                            ErrorKind::DuplicateProperty,
                            span,
                            "duplicate property name",
                        ));
                    }
                }
                self.expect(Kind::Colon, "expected property colon")?;
                let value = self.expression(0)?;
                self.check_aggregate(value, false)?;
                let property = self.wrap(NodeKind::Property(key), span.start, &[value])?;
                self.append(&mut properties, property)?;
                if !self.eat(Kind::Comma)? {
                    break;
                }
            }
            self.expect(Kind::RBrace, "expected closing property bag")?;
        }
        self.add(NodeKind::Properties, start, properties)
    }
    fn relationship(&mut self, create: bool) -> Result<AstId, ParseError> {
        let start = self.span().start;
        let incoming = self.eat(Kind::Lt)?;
        self.expect(Kind::Minus, "expected relationship dash")?;
        let mut variable = None;
        let mut parts = Vec::new();
        let mut bounds = None;
        let mut type_count = 0;
        if self.eat(Kind::LBracket)? {
            if matches!(self.kind(), Some(Kind::Ident | Kind::QuotedName)) {
                variable = Some(self.variable_name()?);
            }
            if self.eat(Kind::Colon)? {
                loop {
                    let name_start = self.span().start;
                    let name = self.name()?;
                    let name = self.leaf(NodeKind::Name(name), name_start)?;
                    self.append(&mut parts, name)?;
                    type_count += 1;
                    if !self.eat(Kind::Pipe)? {
                        break;
                    }
                    // The selected original TCK uses the v9 :A|:B spelling too.
                    self.eat(Kind::Colon)?;
                }
            }
            if self.eat(Kind::Star)? {
                bounds = Some(self.path_bounds()?);
            }
            if self.kind() == Some(Kind::LBrace) {
                let props = self.properties()?;
                self.append(&mut parts, props)?;
            }
            if self.kind() == Some(Kind::Parameter) {
                return Err(self.error(
                    ErrorKind::InvalidParameterUse,
                    "whole-map pattern parameter",
                ));
            }
            self.expect(Kind::RBracket, "expected closing relationship bracket")?;
        }
        self.expect(Kind::Minus, "expected relationship dash")?;
        let outgoing = self.eat(Kind::Gt)?;
        let direction = match (incoming, outgoing) {
            (true, false) => Direction::Incoming,
            (false, true) => Direction::Outgoing,
            (false, false) => Direction::Both,
            (true, true) => {
                return Err(self.error(ErrorKind::Syntax, "relationship points in both directions"));
            }
        };
        if create && (bounds.is_some() || direction == Direction::Both || type_count != 1) {
            return Err(ParseError::new(
                ErrorKind::Unsupported,
                Span {
                    start,
                    end: self.end(),
                },
                "CREATE requires directed fixed relationship of one type",
            ));
        }
        self.add(
            NodeKind::RelationshipPattern {
                variable,
                direction,
                bounds,
            },
            start,
            parts,
        )
    }
    fn path_integer(&mut self) -> Result<u32, ParseError> {
        let span = self.span();
        if self.kind() != Some(Kind::Integer) {
            return Err(self.error(
                ErrorKind::InvalidRange,
                "finite literal path bound required",
            ));
        }
        let id = self.advance()?;
        self.text(id)?
            .parse::<u32>()
            .map_err(|_| ParseError::new(ErrorKind::InvalidRange, span, "path bound overflow"))
    }
    fn path_bounds(&mut self) -> Result<PathBounds, ParseError> {
        let start = self.span().start;
        let lower;
        let upper;
        if self.eat(Kind::DotDot)? {
            lower = 1;
            upper = self.path_integer()?;
        } else {
            lower = self.path_integer()?;
            upper = if self.eat(Kind::DotDot)? {
                self.path_integer()?
            } else {
                lower
            };
        }
        if lower > upper || upper > self.limits.path_hops {
            return Err(ParseError::new(
                ErrorKind::InvalidRange,
                Span {
                    start,
                    end: self.end(),
                },
                "invalid bounded path range",
            ));
        }
        Ok(PathBounds { lower, upper })
    }
    fn update_items(&mut self, remove: bool, start: usize) -> Result<AstId, ParseError> {
        let mut items = Vec::new();
        loop {
            let item_start = self.span().start;
            let variable = self.variable_name()?;
            let item = if self.eat(Kind::Dot)? {
                let property = self.name()?;
                if remove {
                    self.leaf(NodeKind::RemoveProperty { variable, property }, item_start)?
                } else {
                    self.expect(Kind::Eq, "expected property assignment")?;
                    let value = self.expression(0)?;
                    self.check_aggregate(value, false)?;
                    self.wrap(
                        NodeKind::SetProperty { variable, property },
                        item_start,
                        &[value],
                    )?
                }
            } else if self.kind() == Some(Kind::Colon) {
                let mut labels = Vec::new();
                self.labels(&mut labels)?;
                self.add(
                    if remove {
                        NodeKind::RemoveLabels { variable }
                    } else {
                        NodeKind::SetLabels { variable }
                    },
                    item_start,
                    labels,
                )?
            } else {
                return Err(self.error(
                    ErrorKind::Unsupported,
                    "only static property and label updates are supported",
                ));
            };
            self.append(&mut items, item)?;
            if !self.eat(Kind::Comma)? {
                break;
            }
        }
        self.add(
            if remove {
                NodeKind::Remove
            } else {
                NodeKind::Set
            },
            start,
            items,
        )
    }
    fn call(&mut self, start: usize) -> Result<AstId, ParseError> {
        if self.kind() == Some(Kind::LBrace) {
            return Err(self.error(ErrorKind::Unsupported, "CALL subquery outside profile"));
        }
        let prefix = self.name()?;
        if self.text(prefix)? != "ze" {
            return Err(self.name_error(prefix, "unknown procedure namespace"));
        }
        self.expect(Kind::Dot, "expected procedure namespace")?;
        let name = self.name()?;
        let procedure = match self.text(name)? {
            "vector_search" => Procedure::VectorSearch,
            "text_search" => Procedure::TextSearch,
            "hybrid_search" => Procedure::HybridSearch,
            _ => return Err(self.name_error(name, "unknown procedure")),
        };
        self.expect(Kind::LParen, "expected procedure arguments")?;
        let mut parts = self.arguments(Kind::RParen)?;
        for part in &parts {
            self.check_aggregate(*part, false)?;
        }
        self.expect_word("YIELD")?;
        loop {
            let yield_start = self.span().start;
            let name = self.name()?;
            let alias = if self.eat_word("AS")? {
                Some(self.variable_name()?)
            } else {
                None
            };
            let item = self.leaf(NodeKind::Yield { name, alias }, yield_start)?;
            self.append(&mut parts, item)?;
            if !self.eat(Kind::Comma)? {
                break;
            }
        }
        self.add(NodeKind::Call(procedure), start, parts)
    }
    fn projection(&mut self, with: bool) -> Result<AstId, ParseError> {
        let start = self
            .ast
            .tokens
            .get(self.position.saturating_sub(1))
            .map_or(self.span().start, |token| token.span.start);
        let distinct = self.eat_word("DISTINCT")?;
        let mut items = Vec::new();
        loop {
            if items.len() == self.limits.columns {
                return Err(self.error(
                    ErrorKind::Limit(LimitKind::Columns),
                    "projection column limit",
                ));
            }
            let item_start = self.span().start;
            let item = if self.eat(Kind::Star)? {
                self.leaf(NodeKind::Star, item_start)?
            } else {
                let expr = self.expression(0)?;
                let alias = if self.eat_word("AS")? {
                    Some(self.variable_name()?)
                } else {
                    None
                };
                self.check_aggregate(expr, true)?;
                if with
                    && alias.is_none()
                    && !matches!(self.ungroup(expr)?.kind, NodeKind::Variable(_))
                {
                    return Err(ParseError::new(
                        ErrorKind::Unsupported,
                        self.node(expr)?.span,
                        "nontrivial WITH expression requires AS",
                    ));
                }
                self.wrap(NodeKind::ProjectionItem { alias }, item_start, &[expr])?
            };
            self.append(&mut items, item)?;
            if !self.eat(Kind::Comma)? {
                break;
            }
        }
        if self.eat_word("ORDER")? {
            self.expect_word("BY")?;
            loop {
                let order_start = self.span().start;
                let expression = self.expression(0)?;
                self.check_aggregate(expression, false)?;
                let descending = self.eat_word("DESC")?;
                if !descending {
                    self.eat_word("ASC")?;
                }
                let order =
                    self.wrap(NodeKind::Order { descending }, order_start, &[expression])?;
                self.append(&mut items, order)?;
                if !self.eat(Kind::Comma)? {
                    break;
                }
            }
        }
        for (word, kind) in [("SKIP", NodeKind::Skip), ("LIMIT", NodeKind::Limit)] {
            let limit_start = self.span().start;
            if self.eat_word(word)? {
                if !matches!(self.kind(), Some(Kind::Integer | Kind::Parameter)) {
                    return Err(self.error(
                        ErrorKind::InvalidRange,
                        "nonnegative literal or parameter required",
                    ));
                }
                let value = self.prefix()?;
                let limit = self.wrap(kind, limit_start, &[value])?;
                self.append(&mut items, limit)?;
            }
        }
        if with {
            self.where_clause(&mut items)?;
        }
        self.add(NodeKind::Projection { with, distinct }, start, items)
    }
    fn ungroup(&self, mut id: AstId) -> Result<&Node, ParseError> {
        loop {
            let node = self.node(id)?;
            if node.kind != NodeKind::Group {
                return Ok(node);
            }
            id = node
                .children
                .first()
                .copied()
                .ok_or_else(|| self.error(ErrorKind::Syntax, "missing grouped expression"))?;
        }
    }
    fn check_aggregate(&self, id: AstId, top_level: bool) -> Result<(), ParseError> {
        let node = self.ungroup(id)?;
        if !node.aggregate {
            return Ok(());
        }
        if !top_level
            || !matches!(
                node.kind,
                NodeKind::Function {
                    function: Function::Count | Function::Collect,
                    ..
                }
            )
            || node
                .children
                .iter()
                .any(|id| self.ast.node(*id).is_some_and(|child| child.aggregate))
        {
            return Err(ParseError::new(
                ErrorKind::Unsupported,
                node.span,
                "aggregate must be a top-level projection",
            ));
        }
        Ok(())
    }
    fn expression(&mut self, minimum: u8) -> Result<AstId, ParseError> {
        if self.depth == self.limits.depth {
            return Err(self.error(ErrorKind::Limit(LimitKind::Depth), "expression depth limit"));
        }
        self.depth += 1;
        let result = self.expression_inner(minimum);
        self.depth -= 1;
        result
    }
    fn expression_inner(&mut self, minimum: u8) -> Result<AstId, ParseError> {
        let start = self.span().start;
        let mut left = self.prefix()?;
        // Comparisons chain over successive operands, not successive booleans.
        let mut comparison_right = None;
        loop {
            if minimum <= 19 && self.eat(Kind::Dot)? {
                let property = self.name()?;
                left = self.wrap(NodeKind::PropertyAccess(property), start, &[left])?;
                continue;
            }
            if minimum <= 19 && self.eat(Kind::LBracket)? {
                if self.kind() == Some(Kind::DotDot) {
                    return Err(self.error(ErrorKind::Unsupported, "list slice outside profile"));
                }
                let index = self.expression(0)?;
                self.expect(Kind::RBracket, "expected closing list index")?;
                left = self.wrap(NodeKind::Index, start, &[left, index])?;
                continue;
            }
            if minimum <= 18 && self.eat(Kind::Colon)? {
                let mut labels = self.children(&[left])?;
                loop {
                    let label_start = self.span().start;
                    let name = self.name()?;
                    let label = self.leaf(NodeKind::Name(name), label_start)?;
                    self.append(&mut labels, label)?;
                    if !self.eat(Kind::Colon)? {
                        break;
                    }
                }
                left = self.add(NodeKind::LabelPredicate, start, labels)?;
                continue;
            }
            if minimum <= 11 && self.eat_word("IS")? {
                let not = self.eat_word("NOT")?;
                self.expect_word("NULL")?;
                left = self.wrap(
                    NodeKind::Unary(if not {
                        UnaryOp::IsNotNull
                    } else {
                        UnaryOp::IsNull
                    }),
                    start,
                    &[left],
                )?;
                continue;
            }
            let Some((op, precedence, words)) = self.operator() else {
                break;
            };
            if precedence < minimum {
                break;
            }
            self.advance()?;
            if words {
                self.expect_word("WITH")?;
            }
            let right = self.expression(precedence + 1)?;
            if precedence == 9 {
                let operand = comparison_right.unwrap_or(left);
                let compare = self.wrap(
                    NodeKind::Binary(op),
                    self.node(operand)?.span.start,
                    &[operand, right],
                )?;
                left = if comparison_right.is_some() {
                    self.wrap(NodeKind::Binary(BinaryOp::And), start, &[left, compare])?
                } else {
                    compare
                };
                comparison_right = Some(right);
            } else {
                left = self.wrap(NodeKind::Binary(op), start, &[left, right])?;
                comparison_right = None;
            }
        }
        Ok(left)
    }
    fn operator(&self) -> Option<(BinaryOp, u8, bool)> {
        let (op, precedence) = match self.kind()? {
            Kind::Eq => (BinaryOp::Eq, 9),
            Kind::Ne => (BinaryOp::Ne, 9),
            Kind::Lt => (BinaryOp::Lt, 9),
            Kind::Le => (BinaryOp::Le, 9),
            Kind::Gt => (BinaryOp::Gt, 9),
            Kind::Ge => (BinaryOp::Ge, 9),
            Kind::Plus => (BinaryOp::Add, 13),
            Kind::Minus => (BinaryOp::Subtract, 13),
            Kind::Star => (BinaryOp::Multiply, 15),
            Kind::Slash => (BinaryOp::Divide, 15),
            Kind::Percent => (BinaryOp::Remainder, 15),
            Kind::Ident if self.word("OR") => (BinaryOp::Or, 1),
            Kind::Ident if self.word("XOR") => (BinaryOp::Xor, 3),
            Kind::Ident if self.word("AND") => (BinaryOp::And, 5),
            Kind::Ident if self.word("IN") => (BinaryOp::In, 11),
            Kind::Ident if self.word("CONTAINS") => (BinaryOp::Contains, 11),
            Kind::Ident if self.word("STARTS") => return Some((BinaryOp::StartsWith, 11, true)),
            Kind::Ident if self.word("ENDS") => return Some((BinaryOp::EndsWith, 11, true)),
            _ => return None,
        };
        Some((op, precedence, false))
    }
    fn prefix(&mut self) -> Result<AstId, ParseError> {
        let start = self.span().start;
        if self.eat_word("NOT")? {
            let inner = self.expression(7)?;
            return self.wrap(NodeKind::Unary(UnaryOp::Not), start, &[inner]);
        }
        if matches!(self.kind(), Some(Kind::Minus | Kind::Plus)) {
            let negative = self.eat(Kind::Minus)?;
            if !negative {
                self.advance()?;
            }
            // The magnitude of MIN is legal only directly under unary minus.
            if negative
                && self.kind() == Some(Kind::Integer)
                && self.text(TextId(self.position))? == "9223372036854775808"
            {
                self.advance()?;
                return self.leaf(NodeKind::Integer(i64::MIN), start);
            }
            let inner = self.expression(17)?;
            return self.wrap(
                NodeKind::Unary(if negative {
                    UnaryOp::Minus
                } else {
                    UnaryOp::Plus
                }),
                start,
                &[inner],
            );
        }
        if self.eat(Kind::LParen)? {
            let inner = self.expression(0)?;
            self.expect(Kind::RParen, "expected closing expression parenthesis")?;
            return self.wrap(NodeKind::Group, start, &[inner]);
        }
        if self.eat(Kind::LBracket)? {
            if self.list_depth == self.limits.list_depth {
                return Err(
                    self.error(ErrorKind::Limit(LimitKind::ListDepth), "list nesting limit")
                );
            }
            self.list_depth += 1;
            let items = self.arguments(Kind::RBracket)?;
            self.list_depth -= 1;
            return self.add(NodeKind::List, start, items);
        }
        match self.kind() {
            Some(Kind::LBrace) => {
                Err(self.error(ErrorKind::Unsupported, "map expression outside profile"))
            }
            Some(Kind::Integer) => {
                let id = self.advance()?;
                let value = self.text(id)?.parse::<i64>().map_err(|_| {
                    ParseError::new(
                        ErrorKind::InvalidLiteral,
                        Span {
                            start,
                            end: self.end(),
                        },
                        "I64 literal overflow",
                    )
                })?;
                self.leaf(NodeKind::Integer(value), start)
            }
            Some(Kind::Float) => {
                let id = self.advance()?;
                let value = self
                    .text(id)?
                    .parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .ok_or_else(|| {
                        ParseError::new(
                            ErrorKind::InvalidLiteral,
                            Span {
                                start,
                                end: self.end(),
                            },
                            "F64 literal overflow",
                        )
                    })?;
                self.leaf(NodeKind::Float(value), start)
            }
            Some(Kind::String) => {
                let id = self.advance()?;
                self.leaf(NodeKind::String(id), start)
            }
            Some(Kind::Parameter) => {
                let id = self.advance()?;
                if !self
                    .parameters
                    .iter()
                    .any(|other| self.ast.text(*other) == self.ast.text(id))
                {
                    if self.parameters.len() == self.limits.parameters {
                        return Err(self.error(
                            ErrorKind::Limit(LimitKind::Parameters),
                            "named parameter limit",
                        ));
                    }
                    let span = self.span();
                    push(&mut self.parameters, id, self.resources, span)?;
                }
                self.leaf(NodeKind::Parameter(id), start)
            }
            Some(Kind::Ident) if self.word("NULL") => {
                self.advance()?;
                self.leaf(NodeKind::Null, start)
            }
            Some(Kind::Ident) if self.word("TRUE") || self.word("FALSE") => {
                let value = self.word("TRUE");
                self.advance()?;
                self.leaf(NodeKind::Boolean(value), start)
            }
            Some(Kind::Ident | Kind::QuotedName) => {
                if self.kind() == Some(Kind::Ident) && self.reserved() {
                    return Err(self.error(ErrorKind::Unsupported, "expression outside profile"));
                }
                let id = self.advance()?;
                if self.kind() == Some(Kind::LParen)
                    || (self.text(id)? == "ze"
                        && self.kind() == Some(Kind::Dot)
                        && self.peek_kind(2) == Some(Kind::LParen))
                {
                    self.function(id, start)
                } else {
                    self.leaf(NodeKind::Variable(id), start)
                }
            }
            _ => Err(self.error(ErrorKind::Syntax, "expected expression")),
        }
    }
    fn reserved(&self) -> bool {
        [
            "MATCH",
            "OPTIONAL",
            "WHERE",
            "WITH",
            "RETURN",
            "CREATE",
            "SET",
            "REMOVE",
            "DELETE",
            "DETACH",
            "CALL",
            "YIELD",
            "ORDER",
            "BY",
            "SKIP",
            "LIMIT",
            "AS",
            "DISTINCT",
            "UNION",
            "MERGE",
            "UNWIND",
            "CASE",
            "WHEN",
            "THEN",
            "ELSE",
            "END",
            "FOREACH",
            "EXISTS",
            "LOAD",
            "START",
            "USE",
            "PROFILE",
            "EXPLAIN",
            "ALL",
            "ASC",
            "ASCENDING",
            "DESC",
            "DESCENDING",
            "ON",
            "CONSTRAINT",
            "DO",
            "FOR",
            "REQUIRE",
            "UNIQUE",
            "MANDATORY",
            "SCALAR",
            "OF",
            "ADD",
            "DROP",
            "AND",
            "OR",
            "XOR",
            "NOT",
            "IN",
            "IS",
            "CONTAINS",
            "STARTS",
            "ENDS",
        ]
        .iter()
        .any(|word| self.word(word))
    }
    fn arguments(&mut self, closing: Kind) -> Result<Vec<AstId>, ParseError> {
        let mut args = Vec::new();
        if self.eat(closing)? {
            return Ok(args);
        }
        loop {
            let arg = self.expression(0)?;
            self.append(&mut args, arg)?;
            if !self.eat(Kind::Comma)? {
                break;
            }
        }
        self.expect(closing, "expected closing delimiter")?;
        Ok(args)
    }
    fn function(&mut self, name: TextId, start: usize) -> Result<AstId, ParseError> {
        let extension = self.eat(Kind::Dot)?;
        let name = if extension { self.name()? } else { name };
        let text = self.text(name)?;
        let function = if extension {
            match text {
                "node_id" => Function::NodeId,
                "relationship_id" => Function::RelationshipId,
                "stored_text" => Function::StoredText,
                _ => return Err(self.name_error(name, "unknown extension function")),
            }
        } else if text.eq_ignore_ascii_case("count") {
            Function::Count
        } else if text.eq_ignore_ascii_case("collect") {
            Function::Collect
        } else if text.eq_ignore_ascii_case("labels") {
            Function::Labels
        } else if text.eq_ignore_ascii_case("type") {
            Function::Type
        } else if text.eq_ignore_ascii_case("size") {
            Function::Size
        } else {
            return Err(self.name_error(name, "function outside profile"));
        };
        self.expect(Kind::LParen, "expected function arguments")?;
        let distinct = self.eat_word("DISTINCT")?;
        let args = if self.eat(Kind::Star)? {
            if distinct || function != Function::Count {
                return Err(self.error(ErrorKind::Unsupported, "only count(*) permits wildcard"));
            }
            self.expect(Kind::RParen, "expected closing count")?;
            Vec::new()
        } else {
            let args = self.arguments(Kind::RParen)?;
            if args.len() != 1 {
                return Err(self.error(ErrorKind::Syntax, "function requires one argument"));
            }
            args
        };
        if distinct && !matches!(function, Function::Count | Function::Collect) {
            return Err(self.error(
                ErrorKind::Unsupported,
                "DISTINCT only permitted for aggregates",
            ));
        }
        self.add(NodeKind::Function { function, distinct }, start, args)
    }
}
