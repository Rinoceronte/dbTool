//! Scope resolver: walks the full token stream and returns the relations + CTEs
//! visible at the cursor. Frames persist in a flat vector so a FROM clause that
//! appears AFTER the cursor in the same frame (e.g. `SELECT | FROM users`) is
//! still in-scope for column completion at the cursor.

use crate::db::ColumnSchema;

use super::lexer::{Tok, TokKind};
use super::schema_cache::SchemaCache;

#[derive(Debug, Clone)]
pub struct ResolvedRelation {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
    pub columns: Vec<ColumnSchema>,
    pub is_cte: bool,
    pub is_subquery: bool,
}

impl ResolvedRelation {
    pub fn effective_alias(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone)]
pub struct CteBinding {
    pub name: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub relations: Vec<ResolvedRelation>,
    pub ctes: Vec<CteBinding>,
}

// ---------- internals ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FromState {
    Idle,
    Expecting,
    Collected,
}

impl Default for FromState {
    fn default() -> Self { FromState::Idle }
}

#[derive(Debug, Default, Clone)]
struct Frame {
    relations: Vec<ResolvedRelation>,
    ctes: Vec<CteBinding>,
    state: FromState,
    pending: Option<PendingRelation>,
    awaiting_subquery_alias: bool,
    subquery_cols: Option<Vec<String>>,
    cte_name_pending: Option<String>,
    cte_cols_pending: Option<Vec<String>>,
    select_list_idents: Vec<String>,
    select_list_open: bool,
}

#[derive(Debug, Default, Clone)]
struct PendingRelation {
    schema: Option<String>,
    name: Option<String>,
    alias: Option<String>,
    last_was_ident: bool,
    last_was_dot: bool,
    as_expected: bool,
    is_subquery: bool,
}

pub fn resolve(tokens: &[Tok], cursor: usize, cache: &SchemaCache) -> Scope {
    // Persistent frame storage.
    let mut frames: Vec<Frame> = vec![Frame::default()];
    let mut parents: Vec<Option<usize>> = vec![None];
    let mut stack: Vec<usize> = vec![0];
    // Frame index the cursor is inside. Captured the first time we see a token
    // at or past the cursor position; never overwritten.
    let mut cursor_frame: Option<usize> = None;

    for t in tokens.iter() {
        if cursor_frame.is_none() && t.start >= cursor {
            cursor_frame = Some(*stack.last().unwrap());
        }

        // A `;` past the cursor is the start of a different statement; stop
        // walking so it doesn't wipe the cursor frame's relations.
        if t.kind == TokKind::SemiColon && cursor_frame.is_some() {
            break;
        }

        match t.kind {
            TokKind::Whitespace | TokKind::Comment => continue,
            _ => {}
        }

        match t.kind {
            TokKind::LParen => {
                let parent = *stack.last().unwrap();
                let parent_frame = &mut frames[parent];
                let open_subquery = parent_frame.state == FromState::Expecting
                    && parent_frame.pending.is_none();
                if open_subquery {
                    parent_frame.pending = Some(PendingRelation {
                        is_subquery: true,
                        ..Default::default()
                    });
                }
                frames.push(Frame::default());
                parents.push(Some(parent));
                stack.push(frames.len() - 1);
            }
            TokKind::RParen => {
                if stack.len() > 1 {
                    let child_idx = stack.pop().unwrap();
                    let child = frames[child_idx].clone();
                    let parent_idx = *stack.last().unwrap();
                    let parent = &mut frames[parent_idx];
                    if let Some(p) = parent.pending.as_mut() {
                        if p.is_subquery {
                            parent.subquery_cols = Some(child.select_list_idents.clone());
                            parent.awaiting_subquery_alias = true;
                        }
                    }
                    if parent.cte_name_pending.is_some() && parent.cte_cols_pending.is_none() {
                        parent.cte_cols_pending = Some(child.select_list_idents.clone());
                        let name = parent.cte_name_pending.take().unwrap();
                        let cols = parent.cte_cols_pending.take().unwrap_or_default();
                        parent.ctes.push(CteBinding { name, columns: cols });
                    }
                }
            }
            TokKind::SemiColon => {
                let top = *stack.last().unwrap();
                frames[top] = Frame::default();
            }
            TokKind::Comma => {
                let top = *stack.last().unwrap();
                let f = &mut frames[top];
                if matches!(f.state, FromState::Collected) {
                    finalize_pending(f, cache);
                    f.state = FromState::Expecting;
                }
            }
            TokKind::Period => {
                let top = *stack.last().unwrap();
                if let Some(p) = frames[top].pending.as_mut() {
                    if p.last_was_ident && !p.last_was_dot {
                        p.last_was_dot = true;
                        p.last_was_ident = false;
                    }
                }
            }
            TokKind::Word | TokKind::QuotedIdent => {
                let lc = t.lc_value();
                let top = *stack.last().unwrap();
                let f = &mut frames[top];

                match lc.as_str() {
                    "select" => {
                        f.select_list_open = true;
                        f.select_list_idents.clear();
                        f.state = FromState::Idle;
                        continue;
                    }
                    "from" => {
                        f.select_list_open = false;
                        if f.pending.is_some() {
                            f.pending = None;
                        }
                        f.state = FromState::Expecting;
                        continue;
                    }
                    "join" | "inner" | "left" | "right" | "full" | "cross" | "outer" => {
                        if matches!(f.state, FromState::Collected) {
                            finalize_pending(f, cache);
                        }
                        f.state = FromState::Expecting;
                        continue;
                    }
                    "on" | "where" | "group" | "having" | "order" | "limit" | "offset"
                    | "union" | "intersect" | "except" | "returning" => {
                        if matches!(f.state, FromState::Collected) {
                            finalize_pending(f, cache);
                        }
                        f.state = FromState::Idle;
                        f.select_list_open = false;
                        continue;
                    }
                    "with" => {
                        f.cte_name_pending = None;
                        continue;
                    }
                    "as" => {
                        if f.awaiting_subquery_alias {
                            // next ident is alias
                        } else if let Some(p) = f.pending.as_mut() {
                            p.as_expected = true;
                            p.last_was_ident = false;
                            p.last_was_dot = false;
                        }
                        continue;
                    }
                    "insert" | "into" | "update" | "set" | "delete" | "values" => {
                        f.select_list_open = false;
                        f.state = FromState::Idle;
                        continue;
                    }
                    "by" => continue,
                    "distinct" | "all" => continue,
                    _ => {}
                }

                // Subquery-alias materialization.
                if f.awaiting_subquery_alias {
                    let cols_names = f.subquery_cols.take().unwrap_or_default();
                    let cols: Vec<ColumnSchema> = cols_names
                        .iter()
                        .map(|n| ColumnSchema {
                            name: n.clone(),
                            type_name: String::new(),
                            nullable: true,
                            is_primary_key: false,
                        })
                        .collect();
                    f.relations.push(ResolvedRelation {
                        schema: None,
                        name: "(subquery)".to_string(),
                        alias: Some(t.value.clone()),
                        columns: cols,
                        is_cte: false,
                        is_subquery: true,
                    });
                    f.awaiting_subquery_alias = false;
                    f.pending = None;
                    f.state = FromState::Collected;
                    continue;
                }

                // CTE name (tentative) detection.
                if matches!(f.state, FromState::Idle)
                    && f.pending.is_none()
                    && f.cte_name_pending.is_none()
                    && !f.select_list_open
                {
                    f.cte_name_pending = Some(t.value.clone());
                    continue;
                }

                // FROM / JOIN relation accumulation.
                match f.state {
                    FromState::Expecting => {
                        let mut p = f.pending.take().unwrap_or_default();
                        if p.is_subquery {
                            p = PendingRelation::default();
                        }
                        p.name = Some(t.value.clone());
                        p.last_was_ident = true;
                        p.last_was_dot = false;
                        f.pending = Some(p);
                        f.state = FromState::Collected;
                    }
                    FromState::Collected => {
                        if let Some(p) = f.pending.as_mut() {
                            if p.last_was_dot {
                                if let Some(cur_name) = p.name.take() {
                                    p.schema = Some(cur_name);
                                }
                                p.name = Some(t.value.clone());
                                p.last_was_ident = true;
                                p.last_was_dot = false;
                            } else if p.as_expected || p.alias.is_none() {
                                p.alias = Some(t.value.clone());
                                p.as_expected = false;
                                p.last_was_ident = true;
                                p.last_was_dot = false;
                            } else {
                                p.last_was_ident = true;
                                p.last_was_dot = false;
                            }
                        }
                    }
                    FromState::Idle => {
                        if f.select_list_open {
                            f.select_list_idents.push(t.value.clone());
                        }
                    }
                }
            }
            _ => {
                let top = *stack.last().unwrap();
                let f = &mut frames[top];
                if let Some(p) = f.pending.as_mut() {
                    p.last_was_ident = false;
                    p.last_was_dot = false;
                }
            }
        }
    }

    // If we never crossed the cursor (e.g. cursor > last token), it's in the
    // currently open frame.
    if cursor_frame.is_none() {
        cursor_frame = Some(*stack.last().unwrap());
    }
    let cf = cursor_frame.unwrap();

    // Finalize pending relation in the cursor frame if SQL ended mid-clause.
    if frames[cf].state == FromState::Collected {
        let f = &mut frames[cf];
        finalize_pending(f, cache);
    }

    // Build the scope: relations of cursor frame + CTEs from it and all ancestors.
    let mut relations = frames[cf].relations.clone();
    let mut ctes: Vec<CteBinding> = Vec::new();
    let mut cur = Some(cf);
    while let Some(i) = cur {
        ctes.extend(frames[i].ctes.clone());
        cur = parents[i];
    }

    // Cross-link: relations that resolve to a known CTE name pick up its columns.
    for r in relations.iter_mut() {
        if let Some(cte) = ctes.iter().find(|c| c.name.eq_ignore_ascii_case(&r.name)) {
            if r.columns.is_empty() {
                r.columns = cte
                    .columns
                    .iter()
                    .map(|n| ColumnSchema {
                        name: n.clone(),
                        type_name: String::new(),
                        nullable: true,
                        is_primary_key: false,
                    })
                    .collect();
            }
            r.is_cte = true;
        }
    }

    Scope { relations, ctes }
}

fn finalize_pending(f: &mut Frame, cache: &SchemaCache) {
    let Some(p) = f.pending.take() else { return };
    let Some(name) = p.name.clone() else { return };
    let columns = if p.is_subquery {
        Vec::new()
    } else {
        cache
            .resolve_table(p.schema.as_deref(), &name)
            .map(|t| t.columns.clone())
            .unwrap_or_default()
    };
    f.relations.push(ResolvedRelation {
        schema: p.schema,
        name,
        alias: p.alias,
        columns,
        is_cte: false,
        is_subquery: p.is_subquery,
    });
}
