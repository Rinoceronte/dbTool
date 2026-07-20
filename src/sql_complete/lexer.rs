//! Tolerant SQL tokenizer producing tokens with char-offset spans.
//!
//! Unlike `sqlparser::tokenizer`, this never fails on incomplete input
//! (unterminated strings/comments are accepted and consumed to EOF).
//! Char offsets (not byte offsets) match the cursor index that
//! `egui::TextEdit::CCursor.index` returns.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
    Word,          // unquoted identifier or keyword
    QuotedIdent,   // "ident", `ident`, [ident]
    Number,
    String,        // 'literal'
    Period,        // .
    Comma,
    SemiColon,
    LParen,
    RParen,
    Comment,
    Whitespace,
    Punct,         // anything else (operators, etc.)
}

#[derive(Debug, Clone)]
pub struct Tok {
    pub kind: TokKind,
    pub raw: String,
    pub value: String,  // unquoted value (== raw for Word/Number, unquoted for QuotedIdent, inner for String, == raw otherwise)
    pub start: usize,   // char offset (inclusive)
    pub end: usize,     // char offset (exclusive)
}

impl Tok {
    pub fn is_ident(&self) -> bool {
        matches!(self.kind, TokKind::Word | TokKind::QuotedIdent)
    }
    pub fn is_significant(&self) -> bool {
        !matches!(self.kind, TokKind::Whitespace | TokKind::Comment)
    }
    pub fn lc_value(&self) -> String {
        self.value.to_ascii_lowercase()
    }
}

pub fn tokenize(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::with_capacity(chars.len() / 4);
    let mut i = 0usize;

    while i < chars.len() {
        let start = i;
        let c = chars[i];

        // Whitespace
        if c.is_whitespace() {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            out.push(Tok {
                kind: TokKind::Whitespace,
                raw: slice(&chars, start, i),
                value: slice(&chars, start, i),
                start,
                end: i,
            });
            continue;
        }

        // Line comment: -- to EOL
        if c == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push(Tok {
                kind: TokKind::Comment,
                raw: slice(&chars, start, i),
                value: slice(&chars, start, i),
                start,
                end: i,
            });
            continue;
        }

        // Line comment MySQL: # to EOL
        if c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push(Tok {
                kind: TokKind::Comment,
                raw: slice(&chars, start, i),
                value: slice(&chars, start, i),
                start,
                end: i,
            });
            continue;
        }

        // Block comment: /* ... */ (tolerant of missing close)
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            if i + 1 < chars.len() {
                i += 2;
            } else {
                i = chars.len();
            }
            out.push(Tok {
                kind: TokKind::Comment,
                raw: slice(&chars, start, i),
                value: slice(&chars, start, i),
                start,
                end: i,
            });
            continue;
        }

        // Single-quoted string (tolerant: consumes to EOF if unterminated).
        if c == '\'' {
            i += 1;
            let mut inner = String::new();
            while i < chars.len() {
                if chars[i] == '\'' {
                    // Doubled quote escape: ''
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        inner.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1; // consume closing
                    break;
                }
                // Postgres/MySQL backslash escape inside 'E...' strings is dialect-specific;
                // for the purpose of completion we just skip the escape.
                if chars[i] == '\\' && i + 1 < chars.len() {
                    inner.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                inner.push(chars[i]);
                i += 1;
            }
            out.push(Tok {
                kind: TokKind::String,
                raw: slice(&chars, start, i),
                value: inner,
                start,
                end: i,
            });
            continue;
        }

        // Quoted identifier: "ident"
        if c == '"' {
            i += 1;
            let mut inner = String::new();
            while i < chars.len() {
                if chars[i] == '"' {
                    if i + 1 < chars.len() && chars[i + 1] == '"' {
                        inner.push('"');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                inner.push(chars[i]);
                i += 1;
            }
            out.push(Tok {
                kind: TokKind::QuotedIdent,
                raw: slice(&chars, start, i),
                value: inner,
                start,
                end: i,
            });
            continue;
        }

        // Backtick identifier (MySQL): `ident`
        if c == '`' {
            i += 1;
            let mut inner = String::new();
            while i < chars.len() {
                if chars[i] == '`' {
                    if i + 1 < chars.len() && chars[i + 1] == '`' {
                        inner.push('`');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                inner.push(chars[i]);
                i += 1;
            }
            out.push(Tok {
                kind: TokKind::QuotedIdent,
                raw: slice(&chars, start, i),
                value: inner,
                start,
                end: i,
            });
            continue;
        }

        // Bracket identifier (SQL Server): [ident]
        if c == '[' {
            i += 1;
            let mut inner = String::new();
            while i < chars.len() && chars[i] != ']' {
                inner.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            out.push(Tok {
                kind: TokKind::QuotedIdent,
                raw: slice(&chars, start, i),
                value: inner,
                start,
                end: i,
            });
            continue;
        }

        // Word (identifier or keyword): [A-Za-z_][A-Za-z_0-9$]*
        if c.is_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            let raw = slice(&chars, start, i);
            out.push(Tok {
                kind: TokKind::Word,
                value: raw.clone(),
                raw,
                start,
                end: i,
            });
            continue;
        }

        // Number: digits with optional `.` and exponent. Simple lookahead.
        if c.is_ascii_digit()
            || (c == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
        {
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                i += 1;
                if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
                    i += 1;
                }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let raw = slice(&chars, start, i);
            out.push(Tok {
                kind: TokKind::Number,
                value: raw.clone(),
                raw,
                start,
                end: i,
            });
            continue;
        }

        // Single-character punctuation with distinct kinds.
        let kind = match c {
            '.' => TokKind::Period,
            ',' => TokKind::Comma,
            ';' => TokKind::SemiColon,
            '(' => TokKind::LParen,
            ')' => TokKind::RParen,
            _ => TokKind::Punct,
        };
        i += 1;
        let raw = slice(&chars, start, i);
        out.push(Tok {
            kind,
            value: raw.clone(),
            raw,
            start,
            end: i,
        });
    }

    out
}

fn slice(chars: &[char], start: usize, end: usize) -> String {
    chars[start..end].iter().collect()
}

/// The current identifier-prefix under the cursor, and its starting char offset.
/// If the char at cursor-1 is `.`, returns ("", cursor) — an empty prefix at
/// the dot, used for dot-completion.
pub fn word_at_cursor(sql: &str, cursor_char: usize) -> (String, usize) {
    let chars: Vec<char> = sql.chars().collect();
    let mut start = cursor_char.min(chars.len());
    while start > 0 {
        let c = chars[start - 1];
        if c.is_alphanumeric() || c == '_' || c == '$' {
            start -= 1;
        } else {
            break;
        }
    }
    let prefix: String = chars[start..cursor_char.min(chars.len())].iter().collect();
    (prefix, start)
}

/// Return the significant (non-whitespace, non-comment) tokens strictly BEFORE the cursor.
/// If the cursor is in the middle of a word token, that word is NOT included in "before".
pub fn significant_before<'a>(tokens: &'a [Tok], cursor: usize) -> Vec<&'a Tok> {
    tokens
        .iter()
        .filter(|t| t.end <= cursor && t.is_significant())
        .collect()
}

/// Is the cursor inside a string literal or comment token?
pub fn cursor_in_dead_zone(tokens: &[Tok], cursor: usize) -> bool {
    for t in tokens {
        if t.start < cursor && cursor <= t.end && matches!(t.kind, TokKind::String | TokKind::Comment) {
            return true;
        }
    }
    false
}

