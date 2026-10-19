//! Exact, bounded JSON grammar for the pinned format-1 model manifest.

use super::model::{ModelError, valid_path, valid_sha};
use std::collections::BTreeSet;

pub(crate) struct Manifest {
    pub(crate) bundle: String,
    pub(crate) files: Vec<ModelFile>,
}
pub(crate) struct ModelFile {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) sha256: String,
}
fn invalid() -> ModelError {
    ModelError::Invalid("malformed format-1 manifest".into())
}

impl Manifest {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, ModelError> {
        let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
        let mut p = Parser {
            chars: text.chars().peekable(),
        };
        let (mut format, mut name, mut bundle, mut query, mut files) =
            (None, None, None, None, None);
        p.take('{')?;
        loop {
            let key = p.string()?;
            p.take(':')?;
            match key.as_str() {
                "format" if format.is_none() => format = Some(p.number()?),
                "name" if name.is_none() => name = Some(p.string()?),
                "bundle" if bundle.is_none() => bundle = Some(p.string()?),
                "query_coreml" if query.is_none() => {
                    query = Some(if p.peek() == Some('n') {
                        for c in "null".chars() {
                            if p.chars.next() != Some(c) {
                                return Err(invalid());
                            }
                        }
                        None
                    } else {
                        Some(p.string()?)
                    });
                }
                "files" if files.is_none() => files = Some(p.files()?),
                _ => return Err(invalid()),
            }
            if p.peek() == Some('}') {
                p.take('}')?;
                break;
            }
            p.take(',')?;
        }
        if p.peek().is_some() || format != Some(1) {
            return Err(invalid());
        }
        let name = name.ok_or_else(invalid)?;
        let bundle = bundle.ok_or_else(invalid)?;
        let query = query.ok_or_else(invalid)?;
        let files = files.ok_or_else(invalid)?;
        if !valid_path(&name)
            || name.contains('/')
            || bundle != format!("{name}.zem")
            || query
                .as_ref()
                .is_some_and(|q| q != &format!("{name}.mlmodelc"))
        {
            return Err(invalid());
        }
        let mut paths = BTreeSet::new();
        let mut has_bundle = false;
        let mut has_query = false;
        for file in &files {
            if !valid_path(&file.path)
                || file.path == "zeppelin-model.json"
                || file
                    .path
                    .split('/')
                    .next()
                    .is_some_and(|p| p == crate::model::VERIFIED || p == ".zeppelin-verified.tmp")
                || !valid_sha(&file.sha256, 64)
                || !paths.insert(file.path.clone())
            {
                return Err(invalid());
            }
            has_bundle |= file.path == bundle;
            let coreml_prefix = format!("{name}.mlmodelc/");
            if file.path == format!("{name}.mlmodelc") {
                return Err(invalid());
            }
            if file.path.starts_with(&coreml_prefix) {
                if query.is_none() {
                    return Err(invalid());
                }
                has_query = true;
            }
        }
        // A listed file cannot also be a parent directory of another file.
        for path in &paths {
            let mut parent = std::path::Path::new(path).parent();
            while let Some(p) = parent {
                if p.to_str().is_some_and(|s| paths.contains(s)) {
                    return Err(invalid());
                }
                parent = p.parent();
            }
        }
        if !has_bundle || (query.is_some() && !has_query) {
            return Err(invalid());
        }
        Ok(Self { bundle, files })
    }
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}
impl Parser<'_> {
    fn peek(&mut self) -> Option<char> {
        while self
            .chars
            .peek()
            .is_some_and(|c| matches!(c, ' ' | '\n' | '\r' | '\t'))
        {
            self.chars.next();
        }
        self.chars.peek().copied()
    }
    fn take(&mut self, expected: char) -> Result<(), ModelError> {
        self.peek();
        if self.chars.next() == Some(expected) {
            Ok(())
        } else {
            Err(invalid())
        }
    }
    fn hex4(&mut self) -> Result<u32, ModelError> {
        let mut value = 0;
        for _ in 0..4 {
            value = value * 16
                + self
                    .chars
                    .next()
                    .and_then(|c| c.to_digit(16))
                    .ok_or_else(invalid)?;
        }
        Ok(value)
    }
    fn string(&mut self) -> Result<String, ModelError> {
        self.take('"')?;
        let mut s = String::new();
        loop {
            match self.chars.next().ok_or_else(invalid)? {
                '"' => return Ok(s),
                '\\' => {
                    let c = match self.chars.next().ok_or_else(invalid)? {
                        '"' => '"',
                        '\\' => '\\',
                        '/' => '/',
                        'b' => '\u{8}',
                        'f' => '\u{c}',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'u' => {
                            let mut value = self.hex4()?;
                            if (0xd800..=0xdbff).contains(&value) {
                                if self.chars.next() != Some('\\') || self.chars.next() != Some('u')
                                {
                                    return Err(invalid());
                                }
                                let low = self.hex4()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    return Err(invalid());
                                }
                                value = 0x10000 + ((value - 0xd800) << 10) + low - 0xdc00;
                            }
                            char::from_u32(value).ok_or_else(invalid)?
                        }
                        _ => return Err(invalid()),
                    };
                    s.push(c);
                }
                c if c < ' ' => return Err(invalid()),
                c => s.push(c),
            }
        }
    }
    fn number(&mut self) -> Result<u64, ModelError> {
        self.peek();
        let first = self
            .chars
            .next()
            .filter(char::is_ascii_digit)
            .ok_or_else(invalid)?;
        let mut n = u64::from(first.to_digit(10).ok_or_else(invalid)?);
        while self.chars.peek().is_some_and(char::is_ascii_digit) {
            if first == '0' {
                return Err(invalid());
            }
            let digit = self
                .chars
                .next()
                .and_then(|c| c.to_digit(10))
                .ok_or_else(invalid)?;
            n = n
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(digit)))
                .ok_or_else(invalid)?;
        }
        Ok(n)
    }
    fn files(&mut self) -> Result<Vec<ModelFile>, ModelError> {
        self.take('[')?;
        let mut files = Vec::new();
        if self.peek() == Some(']') {
            self.take(']')?;
            return Ok(files);
        }
        loop {
            self.take('{')?;
            let (mut path, mut size, mut sha256) = (None, None, None);
            loop {
                let key = self.string()?;
                self.take(':')?;
                match key.as_str() {
                    "path" if path.is_none() => path = Some(self.string()?),
                    "size" if size.is_none() => size = Some(self.number()?),
                    "sha256" if sha256.is_none() => sha256 = Some(self.string()?),
                    _ => return Err(invalid()),
                }
                if self.peek() == Some('}') {
                    self.take('}')?;
                    break;
                }
                self.take(',')?;
            }
            files.push(ModelFile {
                path: path.ok_or_else(invalid)?,
                size: size.ok_or_else(invalid)?,
                sha256: sha256.ok_or_else(invalid)?,
            });
            if self.peek() == Some(']') {
                self.take(']')?;
                break;
            }
            self.take(',')?;
        }
        Ok(files)
    }
}
