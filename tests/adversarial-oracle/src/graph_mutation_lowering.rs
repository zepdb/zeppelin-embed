//! PG21 primitive mutation-plan observations. This module imports no engine or
//! compiler types; expectations come only from the generated recipe.

#[derive(Clone, Copy, Debug)]
pub struct Recipe {
    pub incoming: bool,
    pub parameter_bits: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Expr {
    I64(i64),
    Slot(u32),
    Parameter(u32),
    Property(Box<Expr>, String),
    RelType(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Item {
    CreateNode(u32, Vec<String>),
    CreateRelationship(u32, Expr, Expr, String),
    SetProperty(Expr, String, Expr),
    RemoveProperty(Expr, String),
    SetLabel(Expr, String, bool),
    Delete(Expr, bool),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub items: Vec<Item>,
    pub eager_count: usize,
    pub limit_zero: bool,
    pub columns: Vec<String>,
    pub dynamic_deleted: bool,
    pub no_return_columns: usize,
    pub source_exact: bool,
    pub parameter_bits: u64,
    pub owner_proof: bool,
}

pub fn expected(recipe: Recipe) -> Observation {
    let a = Expr::Slot(1);
    let relationship = Expr::Slot(2);
    let b = Expr::Slot(3);
    let (source, target) = if recipe.incoming {
        (b.clone(), a.clone())
    } else {
        (a.clone(), b.clone())
    };
    Observation {
        items: vec![
            Item::CreateNode(1, vec!["A".into()]),
            Item::SetProperty(a.clone(), "p".into(), Expr::Slot(0)),
            Item::CreateNode(3, vec!["B".into()]),
            Item::CreateRelationship(2, source, target, "R".into()),
            Item::SetProperty(
                relationship.clone(),
                "q".into(),
                Expr::RelType(Box::new(relationship.clone())),
            ),
            Item::SetProperty(
                b.clone(),
                "x".into(),
                Expr::Property(Box::new(relationship.clone()), "q".into()),
            ),
            Item::SetProperty(
                b.clone(),
                "p".into(),
                Expr::Add(
                    Box::new(Expr::Property(Box::new(b.clone()), "p".into())),
                    Box::new(Expr::I64(1)),
                ),
            ),
            Item::SetProperty(b.clone(), "p".into(), Expr::Slot(0)),
            Item::RemoveProperty(b.clone(), "missing".into()),
            Item::SetLabel(b, "Old".into(), false),
            Item::Delete(relationship, true),
        ],
        eager_count: 4,
        limit_zero: true,
        columns: vec!["b".into(), "frozen".into()],
        dynamic_deleted: true,
        no_return_columns: 0,
        source_exact: true,
        parameter_bits: recipe.parameter_bits,
        owner_proof: true,
    }
}

pub fn check(recipe: Recipe, observed: &Observation) -> Result<(), String> {
    let expected = expected(recipe);
    if expected.items != observed.items {
        return Err("items".into());
    }
    if expected.eager_count != observed.eager_count {
        return Err("eager_count".into());
    }
    if expected.limit_zero != observed.limit_zero {
        return Err("limit_zero".into());
    }
    if expected.columns != observed.columns {
        return Err("columns".into());
    }
    if expected.dynamic_deleted != observed.dynamic_deleted {
        return Err("dynamic_deleted".into());
    }
    if expected.no_return_columns != observed.no_return_columns {
        return Err("no_return_columns".into());
    }
    if expected.source_exact != observed.source_exact {
        return Err("source_exact".into());
    }
    if expected.parameter_bits != observed.parameter_bits {
        return Err("parameter_bits".into());
    }
    if expected.owner_proof != observed.owner_proof {
        return Err("owner_proof".into());
    }
    Ok(())
}
