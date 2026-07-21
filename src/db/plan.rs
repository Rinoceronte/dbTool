//! Execution-plan trees for the visual EXPLAIN view.
//!
//! Each dialect's explain output is normalized into a [`PlanNode`] tree:
//! Postgres `EXPLAIN (FORMAT JSON)`, MySQL `EXPLAIN FORMAT=JSON`,
//! SQLite `EXPLAIN QUERY PLAN`, MSSQL `SET SHOWPLAN_ALL ON`.

use super::{DbKind, ResultSet, Value};

#[derive(Debug, Clone)]
pub struct PlanNode {
    /// e.g. "Seq Scan on users".
    pub label: String,
    /// Interesting per-node properties (Filter, Index Cond, …).
    pub detail: Vec<(String, String)>,
    /// Estimated total cost of this subtree, when the dialect reports one.
    pub cost: Option<f64>,
    /// Estimated rows produced.
    pub rows: Option<f64>,
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: Vec::new(),
            cost: None,
            rows: None,
            children: Vec::new(),
        }
    }

    /// Largest node cost in the tree (drives the relative cost coloring).
    pub fn max_cost(&self) -> f64 {
        let own = self.cost.unwrap_or(0.0);
        self.children
            .iter()
            .map(|c| c.max_cost())
            .fold(own, f64::max)
    }
}

/// The dialect's explain statement for `sql` (the visual-plan run).
pub fn explain_sql(kind: DbKind, sql: &str) -> String {
    match kind {
        DbKind::Postgres => format!("EXPLAIN (FORMAT JSON) {sql}"),
        DbKind::MySql => format!("EXPLAIN FORMAT=JSON {sql}"),
        DbKind::Sqlite => format!("EXPLAIN QUERY PLAN {sql}"),
        DbKind::MsSql => format!("SET SHOWPLAN_ALL ON;\n{sql};\nSET SHOWPLAN_ALL OFF;"),
    }
}

/// Normalize an explain run's result sets into a plan tree.
pub fn parse(kind: DbKind, results: &[ResultSet]) -> Result<PlanNode, String> {
    match kind {
        DbKind::Postgres => parse_pg(results),
        DbKind::MySql => parse_mysql(results),
        DbKind::Sqlite => parse_sqlite(results),
        DbKind::MsSql => parse_mssql(results),
    }
}

fn first_cell_text(results: &[ResultSet]) -> Option<String> {
    results
        .iter()
        .find(|r| !r.rows.is_empty())
        .and_then(|r| r.rows.first())
        .and_then(|row| row.first())
        .map(|v| match v {
            Value::Json(j) => j.to_string(),
            other => other.display(),
        })
}

// ---------------------------------------------------------------------------
// Postgres
// ---------------------------------------------------------------------------

fn parse_pg(results: &[ResultSet]) -> Result<PlanNode, String> {
    let text = first_cell_text(results).ok_or("empty EXPLAIN output")?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("unparseable plan JSON: {e}"))?;
    let plan = json
        .get(0)
        .and_then(|o| o.get("Plan"))
        .ok_or("no Plan object in EXPLAIN output")?;
    Ok(pg_node(plan))
}

fn pg_node(v: &serde_json::Value) -> PlanNode {
    let node_type = v
        .get("Node Type")
        .and_then(|s| s.as_str())
        .unwrap_or("Plan");
    let mut label = node_type.to_string();
    if let Some(rel) = v.get("Relation Name").and_then(|s| s.as_str()) {
        label.push_str(&format!(" on {rel}"));
        if let Some(alias) = v.get("Alias").and_then(|s| s.as_str()) {
            if alias != rel {
                label.push_str(&format!(" ({alias})"));
            }
        }
    } else if let Some(idx) = v.get("Index Name").and_then(|s| s.as_str()) {
        label.push_str(&format!(" using {idx}"));
    }
    let mut node = PlanNode::new(label);
    node.cost = v.get("Total Cost").and_then(|c| c.as_f64());
    node.rows = v.get("Plan Rows").and_then(|c| c.as_f64());
    const INTERESTING: &[&str] = &[
        "Join Type",
        "Index Cond",
        "Filter",
        "Hash Cond",
        "Merge Cond",
        "Sort Key",
        "Group Key",
        "Recheck Cond",
        "Rows Removed by Filter",
        "Actual Total Time",
        "Actual Rows",
        "Startup Cost",
        "Parallel Aware",
    ];
    for key in INTERESTING {
        if let Some(val) = v.get(*key) {
            if !val.is_null() && *key != "Parallel Aware" || val.as_bool() == Some(true) {
                let text = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                node.detail.push(((*key).to_string(), text));
            }
        }
    }
    if let Some(children) = v.get("Plans").and_then(|p| p.as_array()) {
        node.children = children.iter().map(pg_node).collect();
    }
    node
}

// ---------------------------------------------------------------------------
// MySQL
// ---------------------------------------------------------------------------

fn parse_mysql(results: &[ResultSet]) -> Result<PlanNode, String> {
    let text = first_cell_text(results).ok_or("empty EXPLAIN output")?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("unparseable plan JSON: {e}"))?;
    let qb = json
        .get("query_block")
        .ok_or("no query_block in EXPLAIN output")?;
    let mut root = PlanNode::new("query_block");
    if let Some(cost) = qb
        .pointer("/cost_info/query_cost")
        .and_then(json_number)
    {
        root.cost = Some(cost);
    }
    mysql_walk(qb, &mut root);
    Ok(root)
}

fn json_number(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// MySQL's plan JSON nests operators under varied keys — walk generically,
/// materializing "table" objects as nodes and recursing into everything else.
fn mysql_walk(v: &serde_json::Value, parent: &mut PlanNode) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(tbl) = map.get("table_name").and_then(|s| s.as_str()) {
                // This object IS a table access node.
                let access = map
                    .get("access_type")
                    .and_then(|s| s.as_str())
                    .unwrap_or("access");
                let mut node = PlanNode::new(format!("{access} on {tbl}"));
                node.cost = v.pointer("/cost_info/prefix_cost").and_then(json_number);
                node.rows = map.get("rows_examined_per_scan").and_then(json_number);
                for key in [
                    "key",
                    "used_key_parts",
                    "ref",
                    "attached_condition",
                    "using_filesort",
                    "using_temporary_table",
                    "filtered",
                ] {
                    if let Some(val) = map.get(key) {
                        if !val.is_null() {
                            let text = match val {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            node.detail.push((key.to_string(), text));
                        }
                    }
                }
                // Subqueries nested inside this table object.
                for (k, val) in map {
                    if k != "table_name" && matches!(val, serde_json::Value::Object(_) | serde_json::Value::Array(_)) {
                        mysql_walk(val, &mut node);
                    }
                }
                parent.children.push(node);
                return;
            }
            for (key, val) in map {
                match val {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        // Operator wrappers become intermediate nodes.
                        if matches!(
                            key.as_str(),
                            "ordering_operation"
                                | "grouping_operation"
                                | "duplicates_removal"
                                | "materialized_from_subquery"
                                | "query_block"
                                | "union_result"
                                | "windowing"
                        ) {
                            let mut node = PlanNode::new(key.clone());
                            node.cost =
                                val.pointer("/cost_info/query_cost").and_then(json_number);
                            mysql_walk(val, &mut node);
                            parent.children.push(node);
                        } else {
                            mysql_walk(val, parent);
                        }
                    }
                    _ => {}
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                mysql_walk(item, parent);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

fn parse_sqlite(results: &[ResultSet]) -> Result<PlanNode, String> {
    let rs = results
        .iter()
        .find(|r| !r.rows.is_empty())
        .ok_or("empty EXPLAIN QUERY PLAN output")?;
    // Columns: id, parent, notused, detail.
    let as_i64 = |v: &Value| match v {
        Value::Int(i) => *i,
        Value::Text(s) => s.parse().unwrap_or(0),
        _ => 0,
    };
    let mut root = PlanNode::new("QUERY PLAN");
    // (id, node) in insertion order; parents always precede children.
    let mut nodes: Vec<(i64, PlanNode)> = Vec::new();
    let mut edges: Vec<(i64, i64)> = Vec::new();
    for row in &rs.rows {
        if row.len() < 4 {
            continue;
        }
        let id = as_i64(&row[0]);
        let parent = as_i64(&row[1]);
        let detail = row[3].display();
        nodes.push((id, PlanNode::new(detail)));
        edges.push((id, parent));
    }
    // Attach children to parents, deepest-last so we can move safely.
    while let Some((id, node)) = nodes.pop() {
        let parent_id = edges.iter().find(|(i, _)| *i == id).map(|(_, p)| *p).unwrap_or(0);
        // Find the parent still in the list; else it hangs off the root.
        if let Some((_, p)) = nodes.iter_mut().find(|(i, _)| *i == parent_id) {
            p.children.insert(0, node);
        } else {
            root.children.insert(0, node);
        }
    }
    Ok(root)
}

// ---------------------------------------------------------------------------
// MSSQL
// ---------------------------------------------------------------------------

fn parse_mssql(results: &[ResultSet]) -> Result<PlanNode, String> {
    let rs = results
        .iter()
        .find(|r| {
            r.columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case("StmtText"))
        })
        .ok_or("no SHOWPLAN output — is SHOWPLAN permission granted?")?;
    let col = |name: &str| {
        rs.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    };
    let (Some(text_i), Some(node_i), Some(parent_i)) =
        (col("StmtText"), col("NodeId"), col("Parent"))
    else {
        return Err("unexpected SHOWPLAN columns".into());
    };
    let cost_i = col("TotalSubtreeCost");
    let rows_i = col("EstimateRows");
    let op_i = col("PhysicalOp");
    let as_i64 = |v: &Value| match v {
        Value::Int(i) => *i,
        Value::Text(s) => s.trim().parse().unwrap_or(0),
        Value::Float(f) => *f as i64,
        _ => 0,
    };
    let as_f64 = |v: &Value| match v {
        Value::Float(f) => Some(*f),
        Value::Int(i) => Some(*i as f64),
        Value::Text(s) => s.trim().parse().ok(),
        _ => None,
    };
    let mut root = PlanNode::new("Statement");
    let mut nodes: Vec<(i64, PlanNode)> = Vec::new();
    let mut edges: Vec<(i64, i64)> = Vec::new();
    for row in &rs.rows {
        let id = as_i64(&row[node_i]);
        let parent = as_i64(&row[parent_i]);
        let text = row[text_i].display().trim().to_string();
        let mut node = match op_i.map(|i| &row[i]) {
            Some(Value::Text(op)) if !op.trim().is_empty() => {
                let mut n = PlanNode::new(op.trim().to_string());
                n.detail.push(("Statement".into(), text));
                n
            }
            _ => PlanNode::new(text),
        };
        node.cost = cost_i.and_then(|i| as_f64(&row[i]));
        node.rows = rows_i.and_then(|i| as_f64(&row[i]));
        nodes.push((id, node));
        edges.push((id, parent));
    }
    while let Some((id, node)) = nodes.pop() {
        let parent_id = edges.iter().find(|(i, _)| *i == id).map(|(_, p)| *p).unwrap_or(0);
        if let Some((_, p)) = nodes.iter_mut().find(|(i, _)| *i == parent_id) {
            p.children.insert(0, node);
        } else {
            root.children.insert(0, node);
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::types::Column;

    fn text_result(cell: &str) -> ResultSet {
        ResultSet {
            columns: vec![Column { name: "plan".into(), type_name: "text".into() }],
            rows: vec![vec![Value::Text(cell.to_string())]],
            rows_affected: None,
            truncated: false,
        }
    }

    #[test]
    fn pg_plan_parses() {
        let json = r#"[{"Plan": {
            "Node Type": "Hash Join",
            "Total Cost": 100.5,
            "Plan Rows": 500,
            "Hash Cond": "(a.id = b.id)",
            "Plans": [
                {"Node Type": "Seq Scan", "Relation Name": "a", "Total Cost": 30.0, "Plan Rows": 1000},
                {"Node Type": "Hash", "Total Cost": 60.0, "Plans": [
                    {"Node Type": "Index Scan", "Relation Name": "b", "Index Name": "b_pkey", "Total Cost": 55.0}
                ]}
            ]
        }}]"#;
        let plan = parse_pg(&[text_result(json)]).unwrap();
        assert_eq!(plan.label, "Hash Join");
        assert_eq!(plan.children.len(), 2);
        assert_eq!(plan.children[0].label, "Seq Scan on a");
        assert_eq!(plan.children[1].children[0].label, "Index Scan on b");
        assert_eq!(plan.max_cost(), 100.5);
        assert!(plan.detail.iter().any(|(k, _)| k == "Hash Cond"));
    }

    #[test]
    fn mysql_plan_parses() {
        let json = r#"{"query_block": {
            "cost_info": {"query_cost": "12.5"},
            "ordering_operation": {
                "using_filesort": true,
                "table": {
                    "table_name": "users",
                    "access_type": "range",
                    "key": "idx_age",
                    "rows_examined_per_scan": 42,
                    "cost_info": {"prefix_cost": "8.0"}
                }
            }
        }}"#;
        let plan = parse_mysql(&[text_result(json)]).unwrap();
        assert_eq!(plan.cost, Some(12.5));
        assert_eq!(plan.children.len(), 1);
        assert_eq!(plan.children[0].label, "ordering_operation");
        assert_eq!(plan.children[0].children[0].label, "range on users");
        assert_eq!(plan.children[0].children[0].rows, Some(42.0));
    }

    #[test]
    fn sqlite_plan_parses() {
        let rs = ResultSet {
            columns: ["id", "parent", "notused", "detail"]
                .iter()
                .map(|n| Column { name: n.to_string(), type_name: "int".into() })
                .collect(),
            rows: vec![
                vec![Value::Int(2), Value::Int(0), Value::Int(0), Value::Text("SCAN a".into())],
                vec![
                    Value::Int(5),
                    Value::Int(2),
                    Value::Int(0),
                    Value::Text("SEARCH b USING INDEX b_idx".into()),
                ],
            ],
            rows_affected: None,
            truncated: false,
        };
        let plan = parse_sqlite(&[rs]).unwrap();
        assert_eq!(plan.children.len(), 1);
        assert_eq!(plan.children[0].label, "SCAN a");
        assert_eq!(plan.children[0].children[0].label, "SEARCH b USING INDEX b_idx");
    }
}
