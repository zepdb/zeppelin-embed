//! Loads a BEIR corpus from its standard on-disk layout.
//!
//! BEIR ships each corpus as three files:
//!
//! - `corpus.jsonl`   — one document per line: `_id`, `title`, `text`
//! - `queries.jsonl`  — one query per line: `_id`, `text`
//! - `qrels/test.tsv` — `query-id`, `corpus-id`, `score`, tab separated,
//!   with a header line
//!
//! The qrels file has two variants in the wild: with and without the header
//! row. Both are accepted, because a silently-skipped first judgement is a
//! quiet accuracy loss rather than a loud failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::eval::Qrels;

/// One BEIR document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeirDocument {
    /// Corpus-assigned document identifier.
    pub id: String,
    /// Title field; empty when the corpus has none.
    pub title: String,
    /// Body field.
    pub text: String,
}

/// One BEIR query.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeirQuery {
    /// Query identifier, matching the qrels.
    pub id: String,
    /// Query text, to be analyzed by the same pipeline as the documents.
    pub text: String,
}

/// A loaded corpus.
#[derive(Clone, Debug, Default)]
pub struct BeirCorpus {
    /// Corpus name, for the evidence table.
    pub name: String,
    /// Every document.
    pub documents: Vec<BeirDocument>,
    /// Every query, including ones with no judgement.
    pub queries: Vec<BeirQuery>,
    /// Relevance judgements.
    pub qrels: Qrels,
}

/// A corpus that could not be loaded.
#[derive(Debug)]
pub enum BeirError {
    /// A required file was missing or unreadable.
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A JSON line did not parse or lacked a required field.
    Malformed {
        /// The path involved.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// What was wrong.
        reason: String,
    },
}

impl std::fmt::Display for BeirError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Malformed { path, line, reason } => {
                write!(formatter, "{}:{line}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for BeirError {}

fn read(path: &Path) -> Result<String, BeirError> {
    std::fs::read_to_string(path).map_err(|source| BeirError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn field<'json>(
    value: &'json serde_json::Value,
    name: &str,
    path: &Path,
    line: usize,
) -> Result<&'json str, BeirError> {
    value
        .get(name)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| BeirError::Malformed {
            path: path.to_path_buf(),
            line,
            reason: format!("missing string field {name:?}"),
        })
}

/// Loads one corpus from `root/<name>/`.
///
/// # Errors
///
/// Returns [`BeirError`] when a file is absent or a line is malformed. A
/// malformed corpus is never partially loaded: a gate run on half a corpus
/// would report a number that looks like a miss.
pub fn load_corpus(root: &Path, name: &str) -> Result<BeirCorpus, BeirError> {
    let base = root.join(name);

    let corpus_path = base.join("corpus.jsonl");
    let mut documents = Vec::new();
    for (index, line) in read(&corpus_path)?.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|error| BeirError::Malformed {
                path: corpus_path.clone(),
                line: index + 1,
                reason: error.to_string(),
            })?;
        documents.push(BeirDocument {
            id: field(&value, "_id", &corpus_path, index + 1)?.to_owned(),
            title: value
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            text: field(&value, "text", &corpus_path, index + 1)?.to_owned(),
        });
    }

    let queries_path = base.join("queries.jsonl");
    let mut queries = Vec::new();
    for (index, line) in read(&queries_path)?.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|error| BeirError::Malformed {
                path: queries_path.clone(),
                line: index + 1,
                reason: error.to_string(),
            })?;
        queries.push(BeirQuery {
            id: field(&value, "_id", &queries_path, index + 1)?.to_owned(),
            text: field(&value, "text", &queries_path, index + 1)?.to_owned(),
        });
    }

    let qrels_path = base.join("qrels").join("test.tsv");
    let qrels = parse_qrels(&read(&qrels_path)?, &qrels_path)?;

    Ok(BeirCorpus {
        name: name.to_owned(),
        documents,
        queries,
        qrels,
    })
}

/// Parses a BEIR qrels TSV, with or without its header row.
///
/// # Errors
///
/// Returns [`BeirError::Malformed`] for a row that is not three fields with
/// an integer grade.
pub fn parse_qrels(text: &str, path: &Path) -> Result<Qrels, BeirError> {
    let mut qrels: Qrels = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut columns = line.split('\t');
        let (Some(query), Some(document), Some(grade)) =
            (columns.next(), columns.next(), columns.next())
        else {
            return Err(BeirError::Malformed {
                path: path.to_path_buf(),
                line: index + 1,
                reason: String::from("expected three tab-separated columns"),
            });
        };
        // The header row is "query-id\tcorpus-id\tscore"; detect it by the
        // grade failing to parse, but only on the first line.
        //
        // Grades are read as SIGNED. TREC-COVID's qrels carry two `-1`
        // judgements, which in TREC convention mean "explicitly assessed and
        // not relevant" and contribute zero gain — the same treatment
        // `pytrec_eval` applies. Clamping here rather than rejecting is what
        // keeps the number comparable to the published one; rejecting would
        // have silently excluded the whole corpus, which is exactly what
        // happened before this was handled.
        let Ok(signed) = grade.trim().parse::<i64>() else {
            if index == 0 {
                continue;
            }
            return Err(BeirError::Malformed {
                path: path.to_path_buf(),
                line: index + 1,
                reason: format!("grade {grade:?} is not an integer"),
            });
        };
        let grade = u32::try_from(signed.max(0)).unwrap_or(0);
        qrels
            .entry(query.to_owned())
            .or_default()
            .insert(document.to_owned(), grade);
    }
    Ok(qrels)
}
