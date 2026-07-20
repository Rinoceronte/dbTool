//! Given the token stream up to the cursor, classify the completion site.

use super::lexer::{Tok, TokKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Context {
    StatementStart,
    AfterSelectList,                 // SELECT _ ... (before FROM)
    AfterFromJoin,                   // FROM _ / JOIN _ (expecting a table)
    AfterJoinTableNeedOn,            // JOIN tab _  (no ON yet)
    AfterOn,                         // JOIN tab ON _
    AfterWhereOrHaving,              // WHERE _ / HAVING _
    AfterGroupOrOrderBy,             // GROUP BY _ / ORDER BY _
    AfterInsertInto,                 // INSERT INTO _
    InsertColumnList { schema: Option<String>, table: String }, // INSERT INTO t (_)
    AfterUpdate,                     // UPDATE _
    AfterUpdateSet,                  // UPDATE t SET _
    AfterDeleteFrom,                 // DELETE FROM _
    AfterDot { parts: Vec<String> }, // last significant token is '.'; parts = identifiers immediately preceding it
    AfterWith,                       // WITH _ (CTE name)
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    None,
    With,
    Select,
    From,
    Join,
    JoinHasTable,
    On,
    Where,
    GroupOrderBy,
    Having,
    Limit,
    InsertInto,
    InsertColumns,
    InsertValues,
    Update,
    Set,
    DeleteFrom,
}

pub fn detect(sql: &str, tokens: &[Tok], cursor: usize) -> Context {
    // Handle dot completion first.
    if let Some(ctx) = detect_dot(sql, tokens, cursor) {
        return ctx;
    }

    // Walk significant tokens up to cursor maintaining a per-depth clause stack.
    let sig: Vec<&Tok> = tokens
        .iter()
        .filter(|t| t.end <= cursor && t.is_significant())
        .collect();

    if sig.is_empty() {
        return Context::StatementStart;
    }

    let mut stack: Vec<Clause> = vec![Clause::None];
    let mut prev_ident: Option<String> = None; // last identifier seen this clause
    let mut prev_ident_chain: Vec<String> = Vec::new(); // schema.table chain built up

    let mut i = 0;
    while i < sig.len() {
        let t = sig[i];
        let lc = t.lc_value();
        let depth = stack.len() - 1;
        let cur = stack[depth];

        match t.kind {
            TokKind::SemiColon => {
                stack[depth] = Clause::None;
                prev_ident = None;
                prev_ident_chain.clear();
            }
            TokKind::LParen => {
                // If we're in InsertInto and the previous ident was the table name,
                // this opens an InsertColumnList.
                if matches!(cur, Clause::InsertInto) && prev_ident.is_some() {
                    stack.push(Clause::InsertColumns);
                } else if matches!(cur, Clause::InsertValues) {
                    stack.push(Clause::InsertValues);
                } else {
                    // Generic subquery/paren: inherit fresh sub-clause.
                    stack.push(Clause::None);
                }
                prev_ident = None;
                prev_ident_chain.clear();
            }
            TokKind::RParen => {
                if stack.len() > 1 {
                    stack.pop();
                }
                prev_ident = None;
                prev_ident_chain.clear();
            }
            TokKind::Comma => {
                // Stay in same clause but reset identifier chain.
                prev_ident = None;
                prev_ident_chain.clear();
            }
            TokKind::Word => match lc.as_str() {
                "select" => {
                    stack[depth] = Clause::Select;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "from" => {
                    // "DELETE FROM" stays as DeleteFrom; otherwise plain From.
                    stack[depth] = if matches!(cur, Clause::DeleteFrom) {
                        Clause::DeleteFrom
                    } else {
                        Clause::From
                    };
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "join" | "inner" | "left" | "right" | "full" | "cross" | "outer" => {
                    stack[depth] = Clause::Join;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "on" => {
                    stack[depth] = Clause::On;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "where" => {
                    stack[depth] = Clause::Where;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "group" | "order" => {
                    stack[depth] = Clause::GroupOrderBy;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "by" => {
                    // "BY" after GROUP/ORDER keeps us in GroupOrderBy.
                }
                "having" => {
                    stack[depth] = Clause::Having;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "limit" | "offset" => {
                    stack[depth] = Clause::Limit;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "with" => {
                    stack[depth] = Clause::With;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "insert" => {
                    stack[depth] = Clause::InsertInto;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "into" => { /* keep InsertInto */ }
                "values" => {
                    stack[depth] = Clause::InsertValues;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "update" => {
                    stack[depth] = Clause::Update;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "set" => {
                    if matches!(cur, Clause::Update | Clause::None) {
                        stack[depth] = Clause::Set;
                    }
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "delete" => {
                    stack[depth] = Clause::DeleteFrom;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                "union" | "intersect" | "except" => {
                    stack[depth] = Clause::None;
                    prev_ident = None;
                    prev_ident_chain.clear();
                }
                _ => {
                    // Regular identifier. If in Join clause and no prev_ident, this is the join target table.
                    if matches!(cur, Clause::Join) {
                        stack[depth] = Clause::JoinHasTable;
                    }
                    prev_ident = Some(t.value.clone());
                    prev_ident_chain.push(t.value.clone());
                }
            },
            TokKind::QuotedIdent => {
                if matches!(cur, Clause::Join) {
                    stack[depth] = Clause::JoinHasTable;
                }
                prev_ident = Some(t.value.clone());
                prev_ident_chain.push(t.value.clone());
            }
            TokKind::Period => {
                // Chain continues (e.g., schema.table)
            }
            _ => {
                // Operators, numbers, strings, etc — reset chain but keep clause.
                prev_ident = None;
                prev_ident_chain.clear();
            }
        }

        i += 1;
    }

    let depth = stack.len() - 1;
    let cur = stack[depth];

    match cur {
        Clause::None => Context::StatementStart,
        Clause::Select => Context::AfterSelectList,
        Clause::From => Context::AfterFromJoin,
        Clause::Join => Context::AfterFromJoin,
        Clause::JoinHasTable => Context::AfterJoinTableNeedOn,
        Clause::On => Context::AfterOn,
        Clause::Where => Context::AfterWhereOrHaving,
        Clause::GroupOrderBy => Context::AfterGroupOrOrderBy,
        Clause::Having => Context::AfterWhereOrHaving,
        Clause::Limit => Context::Unknown,
        Clause::With => Context::AfterWith,
        Clause::InsertInto => Context::AfterInsertInto,
        Clause::InsertColumns => {
            // prev_ident_chain at parent level held the table. We need to dig it out,
            // but we already cleared on LParen push. Re-scan to find it.
            if let Some((sch, tab)) = find_insert_target(&sig) {
                Context::InsertColumnList { schema: sch, table: tab }
            } else {
                Context::Unknown
            }
        }
        Clause::InsertValues => Context::Unknown,
        Clause::Update => {
            if prev_ident.is_none() {
                Context::AfterUpdate
            } else {
                Context::AfterUpdate
            }
        }
        Clause::Set => Context::AfterUpdateSet,
        Clause::DeleteFrom => Context::AfterDeleteFrom,
    }
}

fn detect_dot(sql: &str, tokens: &[Tok], cursor: usize) -> Option<Context> {
    // Cursor is immediately after a Period token (no chars between).
    let chars: Vec<char> = sql.chars().collect();
    if cursor == 0 || cursor > chars.len() {
        return None;
    }
    // Find the significant token that ends exactly at cursor or whose span contains cursor-1.
    // We consider the PREVIOUS significant token.
    let prev_sig = tokens
        .iter()
        .rev()
        .find(|t| t.end <= cursor && t.is_significant())?;

    // Pattern A: previous significant is `.` → dot-completion.
    if prev_sig.kind == TokKind::Period {
        // Gather up the chain of identifiers + dots that immediately precede this dot.
        let mut parts: Vec<String> = Vec::new();
        let mut expect_ident = true;
        for t in tokens.iter().rev() {
            // Skip the period and anything at or after it.
            if t.end > prev_sig.start {
                continue;
            }
            if !t.is_significant() {
                continue;
            }
            if expect_ident {
                if t.is_ident() {
                    parts.push(t.value.clone());
                    expect_ident = false;
                } else {
                    break;
                }
            } else if t.kind == TokKind::Period {
                expect_ident = true;
            } else {
                break;
            }
        }
        parts.reverse();
        return Some(Context::AfterDot { parts });
    }

    // Pattern B: previous significant is an identifier AND the user has typed some prefix
    // following a dot but before any whitespace. We detect this via scanning: find the
    // most recent '.' between the cursor and the previous non-ident-non-dot token.
    // In that case, classify as AfterDot with the preceding chain.
    // Walk backward from cursor through tokens, collecting trailing ident...dot...ident chain.
    let mut parts: Vec<String> = Vec::new();
    let mut state = 0; // 0 expect ident, 1 expect dot
    let mut saw_dot = false;
    for t in tokens.iter().rev().filter(|t| t.end <= cursor && t.is_significant()) {
        match state {
            0 => {
                if t.is_ident() {
                    parts.push(t.value.clone());
                    state = 1;
                } else {
                    break;
                }
            }
            1 => {
                if t.kind == TokKind::Period {
                    saw_dot = true;
                    state = 0;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    if saw_dot && parts.len() >= 2 {
        // Last part (deepest) is the user's in-progress prefix — drop it and report the rest.
        // But report_context needs the chain BEFORE the prefix.
        parts.reverse();
        parts.pop(); // drop trailing prefix under cursor
        return Some(Context::AfterDot { parts });
    }

    None
}

fn find_insert_target(sig: &[&Tok]) -> Option<(Option<String>, String)> {
    // Scan for INSERT INTO <schema>.<table> or INSERT INTO <table>
    let mut i = 0;
    while i < sig.len() {
        if sig[i].lc_value() == "insert"
            && i + 1 < sig.len()
            && sig[i + 1].lc_value() == "into"
        {
            let mut j = i + 2;
            if j >= sig.len() {
                return None;
            }
            let first = if sig[j].is_ident() {
                sig[j].value.clone()
            } else {
                return None;
            };
            j += 1;
            if j + 1 < sig.len()
                && sig[j].kind == TokKind::Period
                && sig[j + 1].is_ident()
            {
                let tab = sig[j + 1].value.clone();
                return Some((Some(first), tab));
            }
            return Some((None, first));
        }
        i += 1;
    }
    None
}
