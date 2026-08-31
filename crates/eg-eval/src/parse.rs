//! Formula text to an expression tree.
//!
//! `eg-model` deliberately does not parse: shapes and graph edges need the
//! references rewritten and everything else left byte-for-byte alone, which is
//! cheaper and far less likely to be subtly wrong than a full parser.
//! Recomputing a formula is the one job that does need the tree, so it is built
//! here and nowhere else.
//!
//! References are not re-scanned. The lexer walks the spans
//! [`scan_references`] already found and takes each one whole, so a formula's
//! precedents in the evaluator are the same references the graph lifted. A
//! second scanner that disagreed with the first about where `1E5` ends would
//! make the two layers argue about a number neither could defend.
//!
//! What is *not* parsed is as deliberate. An array literal, a structured table
//! reference, a whole-column `A:A`, a sheet-scoped name — each is a construct
//! this crate cannot evaluate, so failing to parse it is the honest answer, and
//! the message says which one it hit. A parser that accepted them and guessed
//! would move the lie from here to the result.
//!
//! [`scan_references`]: eg_model::scan_references

use std::fmt;

use eg_model::formula::scan_references_into;
use eg_model::{CellValue, ErrorKind, ParsedRef, ReferenceSpan};

/// A parsed formula.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A literal: number, string, boolean or error.
    Literal(CellValue),
    /// A cell or range reference, unresolved — it names a sheet by string, and
    /// binding that to a sheet id is the workbook's job, not the parser's.
    Reference {
        parsed: ParsedRef,
        /// The reference exactly as written, for citing it back.
        text: String,
    },
    /// An identifier that is not a reference and not a call: a defined name,
    /// optionally qualified by the sheet whose scope it belongs to.
    Name {
        sheet: Option<String>,
        name: String,
    },
    Unary {
        op: UnaryOp,
        arg: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Call {
        /// Upper-cased, with any `_xlfn.` future-function prefix stripped.
        name: String,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Plus,
    /// Postfix `%`.
    Percent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Pow => "^",
            BinOp::Concat => "&",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
        }
    }
}

/// Why a formula could not be turned into a tree.
///
/// Nearly always "this is a construct the evaluator does not model", not "this
/// is malformed" — the input is a formula Excel itself accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub what: String,
    /// Byte offset into the formula text.
    pub at: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.what, self.at)
    }
}

impl std::error::Error for ParseError {}

/// Parse formula text, with or without a leading `=`.
pub fn parse(formula: &str) -> Result<Expr, ParseError> {
    let text = formula.strip_prefix('=').unwrap_or(formula);
    let mut parser = Parser::new(text);
    let expr = parser.expression(0)?;
    let trailing = parser.peek()?.map(|tok| (tok.start, tok.describe()));
    match trailing {
        Some((start, what)) => Err(parser.error_at(start, format!("trailing {what}"))),
        None => Ok(expr),
    }
}

/// The errors Excel can put in a cell, longest first so that `#N/A` cannot
/// shadow a longer literal starting the same way.
const ERROR_LITERALS: &[&str] = &[
    "#GETTING_DATA",
    "#DIV/0!",
    "#VALUE!",
    "#NAME?",
    "#SPILL!",
    "#NULL!",
    "#CALC!",
    "#NUM!",
    "#REF!",
    "#N/A",
];

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Literal(CellValue),
    Reference(ParsedRef, String),
    Name(Option<String>, String),
    /// An identifier immediately followed by `(`, which the lexer consumes.
    Function(String),
    Open,
    Close,
    Comma,
    Op(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: Kind,
    start: usize,
}

impl Token {
    fn describe(&self) -> String {
        match &self.kind {
            Kind::Literal(v) => format!("literal {}", v.to_display()),
            Kind::Reference(_, text) => format!("reference {text}"),
            Kind::Name(None, name) => format!("name {name}"),
            Kind::Name(Some(sheet), name) => format!("name {sheet}!{name}"),
            Kind::Function(name) => format!("call to {name}"),
            Kind::Open => "'('".to_string(),
            Kind::Close => "')'".to_string(),
            Kind::Comma => "','".to_string(),
            Kind::Op(op) => format!("'{op}'"),
        }
    }
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    /// Reference spans in order of appearance, and how far we have consumed
    /// them. The lexer never re-scans a reference; it recognises one by
    /// arriving at its start.
    refs: Vec<ReferenceSpan>,
    next_ref: usize,
    peeked: Option<Option<Token>>,
    /// Nesting, counted so that a pathological formula is refused rather than
    /// recursed into the stack. Formulas are input, and a crash is not an
    /// answer.
    depth: u32,
}

/// How deep an expression may nest before it is refused. Excel's own limit is
/// 64 levels of function nesting.
pub const MAX_DEPTH: u32 = 128;

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        let mut refs = Vec::new();
        scan_references_into(src, &mut refs);
        Self {
            src,
            pos: 0,
            refs,
            next_ref: 0,
            peeked: None,
            depth: 0,
        }
    }

    fn error_at(&self, at: usize, what: impl Into<String>) -> ParseError {
        ParseError {
            what: what.into(),
            at,
        }
    }

    fn error(&self, what: impl Into<String>) -> ParseError {
        self.error_at(self.pos, what)
    }

    fn peek(&mut self) -> Result<Option<&Token>, ParseError> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lex()?);
        }
        Ok(self.peeked.as_ref().and_then(|t| t.as_ref()))
    }

    fn next(&mut self) -> Result<Option<Token>, ParseError> {
        match self.peeked.take() {
            Some(tok) => Ok(tok),
            None => self.lex(),
        }
    }

    fn eat_op(&mut self, op: &str) -> Result<bool, ParseError> {
        let hit = matches!(self.peek()?, Some(Token { kind: Kind::Op(o), .. }) if *o == op);
        if hit {
            self.next()?;
        }
        Ok(hit)
    }

    // ---- lexer ----------------------------------------------------------

    fn lex(&mut self) -> Result<Option<Token>, ParseError> {
        let b = self.src.as_bytes();
        while self.pos < b.len() && b[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        if self.pos >= b.len() {
            return Ok(None);
        }
        let start = self.pos;

        // A reference the scanner already found, taken whole. Spans are in
        // order, so anything ending before here is behind us.
        while self.next_ref < self.refs.len() && self.refs[self.next_ref].span.start < start {
            self.next_ref += 1;
        }
        if self.next_ref < self.refs.len() && self.refs[self.next_ref].span.start == start {
            let span = self.refs[self.next_ref].clone();
            self.next_ref += 1;
            self.pos = span.span.end;
            let text = self.src[span.span.clone()].to_string();
            return Ok(Some(Token {
                kind: Kind::Reference(span.parsed, text),
                start,
            }));
        }

        let c = b[self.pos];
        let kind = match c {
            b'"' => Kind::Literal(CellValue::Text(self.string_literal()?)),
            b'#' => Kind::Literal(CellValue::Error(self.error_literal()?)),
            b'0'..=b'9' => Kind::Literal(CellValue::Number(self.number()?)),
            b'.' if b.get(self.pos + 1).is_some_and(u8::is_ascii_digit) => {
                Kind::Literal(CellValue::Number(self.number()?))
            }
            b'(' => {
                self.pos += 1;
                Kind::Open
            }
            b')' => {
                self.pos += 1;
                Kind::Close
            }
            b',' => {
                self.pos += 1;
                Kind::Comma
            }
            b'{' => return Err(self.error("array literal")),
            b'[' => return Err(self.error("structured or external reference")),
            // A quoted sheet name the reference scanner did not claim. What
            // follows is a name scoped to that sheet, not a reference.
            b'\'' => self.quoted_qualified_name()?,
            b';' => return Err(self.error("';' argument separator")),
            b':' => return Err(self.error("range operator on something that is not a reference")),
            b'<' | b'>' => {
                // One byte of lookahead, not a two-byte string slice:
                // `self.pos + 2` can land *inside* a multi-byte character —
                // `A1<é`, a comparison against a defined name that does not
                // start with an ASCII letter — and slicing at a non-boundary
                // panics rather than merely failing to match. Same trap
                // `error_literal` guards against with `rest.get(..n)`, and a
                // panic here would take down a whole `eg check` sweep, or the
                // MCP server process, on one formula.
                let op = match (c, b.get(self.pos + 1)) {
                    (b'<', Some(b'=')) => "<=",
                    (b'>', Some(b'=')) => ">=",
                    (b'<', Some(b'>')) => "<>",
                    (b'<', _) => "<",
                    _ => ">",
                };
                self.pos += op.len();
                Kind::Op(op)
            }
            b'+' | b'-' | b'*' | b'/' | b'^' | b'&' | b'=' | b'%' => {
                self.pos += 1;
                Kind::Op(match c {
                    b'+' => "+",
                    b'-' => "-",
                    b'*' => "*",
                    b'/' => "/",
                    b'^' => "^",
                    b'&' => "&",
                    b'=' => "=",
                    _ => "%",
                })
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'\\' => self.identifier()?,
            other => return Err(self.error(format!("unexpected byte {:?}", other as char))),
        };
        Ok(Some(Token { kind, start }))
    }

    fn string_literal(&mut self) -> Result<String, ParseError> {
        let b = self.src.as_bytes();
        let start = self.pos;
        let mut out = String::new();
        let mut i = self.pos + 1;
        loop {
            match b.get(i) {
                None => return Err(self.error_at(start, "unterminated string")),
                Some(b'"') if b.get(i + 1) == Some(&b'"') => {
                    out.push('"');
                    i += 2;
                }
                Some(b'"') => {
                    i += 1;
                    break;
                }
                Some(_) => {
                    let ch = self.src[i..].chars().next().expect("byte is on a boundary");
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        self.pos = i;
        Ok(out)
    }

    fn error_literal(&mut self) -> Result<ErrorKind, ParseError> {
        let rest = &self.src[self.pos..];
        for literal in ERROR_LITERALS {
            // `rest.get(..n)` rather than `rest[..n]`: every literal is ASCII,
            // but `rest` need not be — a multibyte character early in the
            // text can put `literal.len()` mid-character, and a byte slice at
            // a non-boundary panics rather than just failing to match.
            if rest
                .get(..literal.len())
                .is_some_and(|c| c.eq_ignore_ascii_case(literal))
            {
                self.pos += literal.len();
                return Ok(ErrorKind::parse(literal).expect("literal table matches ErrorKind"));
            }
        }
        Err(self.error("unknown error literal"))
    }

    fn number(&mut self) -> Result<f64, ParseError> {
        let b = self.src.as_bytes();
        let start = self.pos;
        let mut i = self.pos;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
            i += 1;
        }
        // An exponent, but only a complete one: `1E` is not a number, and in
        // `1+E5` the `E5` belongs to the reference scanner, not to us.
        if i < b.len() && (b[i] | 0x20) == b'e' {
            let mut j = i + 1;
            if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                j += 1;
            }
            if j < b.len() && b[j].is_ascii_digit() {
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                i = j;
            }
        }
        let text = &self.src[start..i];
        let value = text
            .parse::<f64>()
            .map_err(|_| self.error_at(start, format!("malformed number {text}")))?;
        self.pos = i;
        Ok(value)
    }

    fn identifier(&mut self) -> Result<Kind, ParseError> {
        let b = self.src.as_bytes();
        let start = self.pos;
        // Excel allows a name to begin with `\`, but only there.
        let mut i = if b[start] == b'\\' { start + 1 } else { start };
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
            i += 1;
        }
        let word = &self.src[start..i];
        if i < b.len() && b[i] == b'!' {
            self.pos = i + 1;
            let name = self.plain_name(start)?;
            return Ok(Kind::Name(Some(word.to_string()), name));
        }
        self.pos = i;
        if i < b.len() && b[i] == b'(' {
            self.pos = i + 1;
            let name = word
                .strip_prefix("_xlfn.")
                .unwrap_or(word)
                .to_ascii_uppercase();
            return Ok(Kind::Function(name));
        }
        Ok(Kind::Name(None, word.to_string()))
    }

    /// `'Q3 Sales'!Tax_Rate` — a name scoped to a sheet whose own name needs
    /// quoting.
    fn quoted_qualified_name(&mut self) -> Result<Kind, ParseError> {
        let b = self.src.as_bytes();
        let start = self.pos;
        let mut i = start + 1;
        loop {
            match b.get(i) {
                None => return Err(self.error_at(start, "unterminated sheet name")),
                Some(b'\'') if b.get(i + 1) == Some(&b'\'') => i += 2,
                Some(b'\'') => break,
                Some(_) => i += 1,
            }
        }
        let sheet = self.src[start + 1..i].replace("''", "'");
        if b.get(i + 1) != Some(&b'!') {
            return Err(self.error_at(start, "quoted sheet name without a reference"));
        }
        self.pos = i + 2;
        let name = self.plain_name(start)?;
        Ok(Kind::Name(Some(sheet), name))
    }

    /// The bare identifier after a `!`, which is a name and never a call.
    fn plain_name(&mut self, start: usize) -> Result<String, ParseError> {
        let b = self.src.as_bytes();
        let from = self.pos;
        let mut i = from;
        if b.get(i) == Some(&b'\\') {
            i += 1;
        }
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
            i += 1;
        }
        if i == from {
            return Err(self.error_at(start, "sheet qualifier followed by nothing nameable"));
        }
        if b.get(i) == Some(&b'(') {
            return Err(self.error_at(start, "sheet-qualified function call"));
        }
        self.pos = i;
        Ok(self.src[from..i].to_string())
    }

    // ---- parser ---------------------------------------------------------

    /// Precedence-climbing over Excel's operator table, loosest binding first:
    /// comparison, `&`, `+ -`, `* /`, `^`, unary sign, postfix `%`.
    fn expression(&mut self, min_binding: u8) -> Result<Expr, ParseError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error(format!("nested more than {MAX_DEPTH} deep")));
        }
        self.depth += 1;
        let out = self.expression_inner(min_binding);
        self.depth -= 1;
        out
    }

    fn expression_inner(&mut self, min_binding: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.unary()?;
        while let Some(Token {
            kind: Kind::Op(op), ..
        }) = self.peek()?
        {
            let (op, binding, left_associative) = match *op {
                "=" => (BinOp::Eq, 1, true),
                "<>" => (BinOp::Ne, 1, true),
                "<" => (BinOp::Lt, 1, true),
                "<=" => (BinOp::Le, 1, true),
                ">" => (BinOp::Gt, 1, true),
                ">=" => (BinOp::Ge, 1, true),
                "&" => (BinOp::Concat, 2, true),
                "+" => (BinOp::Add, 3, true),
                "-" => (BinOp::Sub, 3, true),
                "*" => (BinOp::Mul, 4, true),
                "/" => (BinOp::Div, 4, true),
                // `2^3^2` is 512 in Excel: right-associative, unlike the rest.
                "^" => (BinOp::Pow, 5, false),
                _ => break,
            };
            if binding < min_binding {
                break;
            }
            self.next()?;
            let next_min = if left_associative {
                binding + 1
            } else {
                binding
            };
            let rhs = self.expression(next_min)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// A sign binds tighter than `^`, which is why it is handled here and not
    /// in the operator table: `-2^2` is 4 in Excel, not -4.
    fn unary(&mut self) -> Result<Expr, ParseError> {
        if self.eat_op("-")? {
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                arg: Box::new(self.unary()?),
            });
        }
        if self.eat_op("+")? {
            return Ok(Expr::Unary {
                op: UnaryOp::Plus,
                arg: Box::new(self.unary()?),
            });
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.primary()?;
        while self.eat_op("%")? {
            expr = Expr::Unary {
                op: UnaryOp::Percent,
                arg: Box::new(expr),
            };
        }
        Ok(expr)
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let Some(token) = self.next()? else {
            return Err(self.error("formula ends where a value was expected"));
        };
        match token.kind {
            Kind::Literal(value) => Ok(Expr::Literal(value)),
            Kind::Reference(parsed, text) => Ok(Expr::Reference { parsed, text }),
            Kind::Name(sheet, name) => Ok(match (&sheet, name.to_ascii_uppercase().as_str()) {
                (None, "TRUE") => Expr::Literal(CellValue::Bool(true)),
                (None, "FALSE") => Expr::Literal(CellValue::Bool(false)),
                _ => Expr::Name { sheet, name },
            }),
            Kind::Open => {
                let inner = self.expression(0)?;
                self.expect_close()?;
                Ok(inner)
            }
            Kind::Function(name) => {
                let args = self.arguments()?;
                Ok(Expr::Call { name, args })
            }
            other => Err(self.error_at(
                token.start,
                format!("{} where a value was expected", other_describe(&other)),
            )),
        }
    }

    /// The argument list of a call whose `(` the lexer has consumed.
    fn arguments(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut args = Vec::new();
        if matches!(
            self.peek()?,
            Some(Token {
                kind: Kind::Close,
                ..
            })
        ) {
            self.next()?;
            return Ok(args);
        }
        loop {
            // `IF(A1,,2)` — an omitted argument is a real thing and means the
            // default, which is not the same as zero for every function.
            args.push(match self.peek()? {
                Some(Token {
                    kind: Kind::Comma, ..
                })
                | Some(Token {
                    kind: Kind::Close, ..
                }) => Expr::Literal(CellValue::Empty),
                _ => self.expression(0)?,
            });
            match self.next()? {
                Some(Token {
                    kind: Kind::Comma, ..
                }) => continue,
                Some(Token {
                    kind: Kind::Close, ..
                }) => return Ok(args),
                Some(other) => {
                    return Err(self.error_at(
                        other.start,
                        format!("{} inside an argument list", other.describe()),
                    ))
                }
                None => return Err(self.error("unclosed argument list")),
            }
        }
    }

    fn expect_close(&mut self) -> Result<(), ParseError> {
        match self.next()? {
            Some(Token {
                kind: Kind::Close, ..
            }) => Ok(()),
            Some(other) => Err(self.error_at(
                other.start,
                format!("{} where ')' was expected", other.describe()),
            )),
            None => Err(self.error("unclosed '('")),
        }
    }
}

fn other_describe(kind: &Kind) -> String {
    Token {
        kind: kind.clone(),
        start: 0,
    }
    .describe()
}
