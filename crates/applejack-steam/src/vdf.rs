//! Valve's text `KeyValues` format, as used by `appmanifest_<appid>.acf`,
//! `libraryfolders.vdf`, and `config.vdf`.
//!
//! The reader accepts quoted and bare tokens, nested braces up to
//! [`MAX_DEPTH`] levels, `//` comments, a leading UTF-8 BOM, and the escapes
//! `\"`, `\\`, `\n`, `\t`, and `\r`; any other escape is an error. A
//! `[$CONDITION]` suffix is skipped (never evaluated) when it follows a value
//! on the same line, or a key whose value is a `{` block; a `[` anywhere else
//! is part of a bare token, so `a[b]` is one token while `v [b]` is a value
//! and a conditional. Top-level `#base` and `#include` directives are
//! rejected. Duplicate keys are kept in order.
//!
//! The writer emits Valve's layout (tab indentation, `"key"<tab><tab>"value"`,
//! braces on their own lines, LF line ends). A client-written file (LF line
//! ends, no BOM, control characters inside quoted strings escaped, which is
//! what Valve's writer produces) round-trips byte for byte. Input the client
//! does not write is normalised: CRLF between tokens becomes LF, a leading
//! BOM is dropped, and a raw tab, carriage return, or newline inside a quoted
//! string is written back as its escape.

use crate::Error;

/// Deepest `{` nesting the reader accepts. Valve's own files nest a handful
/// of levels; anything deeper is not a client file, and bounding the
/// recursion keeps a malformed file from overflowing the stack.
pub const MAX_DEPTH: usize = 64;

/// One `"key" value` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub key: String,
    pub value: Value,
}

impl Node {
    #[must_use]
    pub fn str(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: Value::Str(value.into()),
        }
    }

    #[must_use]
    pub fn map(key: impl Into<String>, children: Vec<Node>) -> Self {
        Self {
            key: key.into(),
            value: Value::Map(children),
        }
    }
}

/// A leaf string or an ordered map of child nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Map(Vec<Node>),
}

impl Value {
    /// Follows a `/`-separated path of keys, comparing ASCII case-insensitively
    /// like Valve's `KeyValues::FindKey`. The first matching key wins when a
    /// map holds duplicates.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Value> {
        path.split('/').try_fold(self, |value, key| match value {
            Value::Map(children) => find(children, key),
            Value::Str(_) => None,
        })
    }

    /// Mutable counterpart of [`Value::get`]; `None` when any segment is
    /// missing.
    #[must_use]
    pub fn get_mut(&mut self, path: &str) -> Option<&mut Value> {
        path.split('/').try_fold(self, |value, key| match value {
            Value::Map(children) => find_mut(children, key),
            Value::Str(_) => None,
        })
    }

    /// The leaf text; `None` for a map.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(text) => Some(text),
            Value::Map(_) => None,
        }
    }

    /// The child nodes; `None` for a leaf.
    #[must_use]
    pub fn as_map(&self) -> Option<&[Node]> {
        match self {
            Value::Map(children) => Some(children),
            Value::Str(_) => None,
        }
    }

    #[must_use]
    pub fn get_str(&self, path: &str) -> Option<&str> {
        self.get(path)?.as_str()
    }

    #[must_use]
    pub fn get_map(&self, path: &str) -> Option<&[Node]> {
        self.get(path)?.as_map()
    }

    #[must_use]
    pub fn get_u32(&self, path: &str) -> Option<u32> {
        self.get_str(path)?.parse().ok()
    }

    #[must_use]
    pub fn get_u64(&self, path: &str) -> Option<u64> {
        self.get_str(path)?.parse().ok()
    }
}

fn find<'a>(children: &'a [Node], key: &str) -> Option<&'a Value> {
    children
        .iter()
        .find(|node| node.key.eq_ignore_ascii_case(key))
        .map(|node| &node.value)
}

fn find_mut<'a>(children: &'a mut [Node], key: &str) -> Option<&'a mut Value> {
    children
        .iter_mut()
        .find(|node| node.key.eq_ignore_ascii_case(key))
        .map(|node| &mut node.value)
}

fn split_first(path: &str) -> (&str, Option<&str>) {
    match path.split_once('/') {
        Some((first, rest)) => (first, Some(rest)),
        None => (path, None),
    }
}

/// A parsed file: its top-level nodes in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    pub nodes: Vec<Node>,
}

impl Document {
    /// Parses `KeyValues` text.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VdfParse`] with the line of the first malformed token.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut parser = Parser {
            chars: text.char_indices().peekable(),
            text,
            line: 1,
        };
        let nodes = parser.nodes(0)?;
        Ok(Self { nodes })
    }

    /// Renders the document in Valve's layout.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for node in &self.nodes {
            write_node(&mut out, node, 0);
        }
        out
    }

    /// Looks a path up across the top-level nodes; see [`Value::get`].
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Value> {
        let (first, rest) = split_first(path);
        let root = find(&self.nodes, first)?;
        match rest {
            Some(rest) => root.get(rest),
            None => Some(root),
        }
    }

    /// Mutable counterpart of [`Document::get`]; `None` when any segment is
    /// missing.
    #[must_use]
    pub fn get_mut(&mut self, path: &str) -> Option<&mut Value> {
        let (first, rest) = split_first(path);
        let root = find_mut(&mut self.nodes, first)?;
        match rest {
            Some(rest) => root.get_mut(rest),
            None => Some(root),
        }
    }

    #[must_use]
    pub fn get_str(&self, path: &str) -> Option<&str> {
        self.get(path)?.as_str()
    }

    #[must_use]
    pub fn get_map(&self, path: &str) -> Option<&[Node]> {
        self.get(path)?.as_map()
    }

    #[must_use]
    pub fn get_u32(&self, path: &str) -> Option<u32> {
        self.get_str(path)?.parse().ok()
    }

    #[must_use]
    pub fn get_u64(&self, path: &str) -> Option<u64> {
        self.get_str(path)?.parse().ok()
    }
}

fn write_node(out: &mut String, node: &Node, depth: usize) {
    indent(out, depth);
    write_quoted(out, &node.key);
    match &node.value {
        Value::Str(text) => {
            out.push_str("\t\t");
            write_quoted(out, text);
            out.push('\n');
        }
        Value::Map(children) => {
            out.push('\n');
            indent(out, depth);
            out.push_str("{\n");
            for child in children {
                write_node(out, child, depth + 1);
            }
            indent(out, depth);
            out.push_str("}\n");
        }
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push('\t');
    }
}

fn write_quoted(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
}

#[derive(Debug)]
enum Token {
    Open,
    Close,
    Text(String),
    End,
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    text: &'a str,
    line: usize,
}

impl Parser<'_> {
    fn error(&self, message: impl Into<String>) -> Error {
        Error::VdfParse {
            line: self.line,
            message: message.into(),
        }
    }

    fn nodes(&mut self, depth: usize) -> Result<Vec<Node>, Error> {
        if depth > MAX_DEPTH {
            return Err(self.error(format!("nesting deeper than {MAX_DEPTH} levels")));
        }
        let mut nodes = Vec::new();
        loop {
            let key = match self.token()? {
                Token::Text(key) => key,
                Token::Close if depth > 0 => return Ok(nodes),
                Token::Close => return Err(self.error("unexpected `}` at top level")),
                Token::End if depth == 0 => return Ok(nodes),
                Token::End => return Err(self.error("unexpected end of input inside `{`")),
                Token::Open => return Err(self.error("expected a key before `{`")),
            };
            if depth == 0
                && (key.eq_ignore_ascii_case("#base") || key.eq_ignore_ascii_case("#include"))
            {
                return Err(self.error(format!("`{key}` directives are not supported")));
            }
            let key_line = self.line;
            let key_conditional = self.conditional_on_line()?;
            let value = match self.token()? {
                Token::Text(_) if key_conditional => {
                    return Err(Error::VdfParse {
                        line: key_line,
                        message: format!("conditional after key `{key}` must be followed by `{{`"),
                    });
                }
                Token::Text(text) => {
                    self.conditional_on_line()?;
                    Value::Str(text)
                }
                Token::Open => Value::Map(self.nodes(depth + 1)?),
                Token::Close => return Err(self.error(format!("key `{key}` has no value"))),
                Token::End => {
                    return Err(self.error(format!("key `{key}` has no value at end of input")));
                }
            };
            nodes.push(Node { key, value });
        }
    }

    fn token(&mut self) -> Result<Token, Error> {
        loop {
            let Some(&(index, ch)) = self.chars.peek() else {
                return Ok(Token::End);
            };
            match ch {
                '\n' => {
                    self.line += 1;
                    self.chars.next();
                }
                ch if ch.is_whitespace() => {
                    self.chars.next();
                }
                '/' if self.text[index..].starts_with("//") => self.skip_line(),
                '{' => {
                    self.chars.next();
                    return Ok(Token::Open);
                }
                '}' => {
                    self.chars.next();
                    return Ok(Token::Close);
                }
                '"' => {
                    self.chars.next();
                    return self.quoted().map(Token::Text);
                }
                _ => return Ok(Token::Text(self.bare())),
            }
        }
    }

    fn skip_line(&mut self) {
        for (_, ch) in self.chars.by_ref() {
            if ch == '\n' {
                self.line += 1;
                break;
            }
        }
    }

    /// Skips a `[...]` conditional that follows the previous token on the
    /// same line. Returns whether one was present.
    fn conditional_on_line(&mut self) -> Result<bool, Error> {
        while let Some(&(_, ch)) = self.chars.peek() {
            match ch {
                ' ' | '\t' | '\r' => {
                    self.chars.next();
                }
                '[' => {
                    self.skip_conditional()?;
                    return Ok(true);
                }
                _ => break,
            }
        }
        Ok(false)
    }

    fn skip_conditional(&mut self) -> Result<(), Error> {
        for (_, ch) in self.chars.by_ref() {
            match ch {
                ']' => return Ok(()),
                '\n' => break,
                _ => {}
            }
        }
        Err(self.error("unterminated `[` conditional"))
    }

    fn quoted(&mut self) -> Result<String, Error> {
        let mut out = String::new();
        while let Some((_, ch)) = self.chars.next() {
            match ch {
                '"' => return Ok(out),
                '\\' => match self.chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, 'r')) => out.push('\r'),
                    Some((_, '"')) => out.push('"'),
                    Some((_, '\\')) => out.push('\\'),
                    // Rejected rather than kept verbatim: the writer would
                    // double the backslash and change the bytes on rewrite.
                    Some((_, other)) => {
                        return Err(self.error(format!("unknown escape `\\{other}`")));
                    }
                    None => return Err(self.error("unterminated string")),
                },
                '\n' => {
                    self.line += 1;
                    out.push('\n');
                }
                other => out.push(other),
            }
        }
        Err(self.error("unterminated string"))
    }

    fn bare(&mut self) -> String {
        let mut out = String::new();
        while let Some(&(_, ch)) = self.chars.peek() {
            if ch.is_whitespace() || matches!(ch, '{' | '}' | '"') {
                break;
            }
            out.push(ch);
            self.chars.next();
        }
        out
    }
}

impl std::fmt::Display for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_text())
    }
}
