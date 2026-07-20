use dbtool::db::{ColumnSchema, DbKind, DbMeta, ForeignKey, TableKind, TableMeta};
use dbtool::sql_complete::{complete, CompletionRequest, ItemKind, SchemaCache};

fn col(name: &str, ty: &str, pk: bool) -> ColumnSchema {
    ColumnSchema {
        name: name.to_string(),
        type_name: ty.to_string(),
        nullable: !pk,
        is_primary_key: pk,
    }
}

fn fixture() -> SchemaCache {
    let users = TableMeta {
        schema: "public".to_string(),
        name: "users".to_string(),
        kind: TableKind::Table,
        columns: vec![
            col("id", "int8", true),
            col("email", "text", false),
            col("name", "text", false),
            col("created_at", "timestamptz", false),
        ],
        primary_key: vec!["id".into()],
        foreign_keys: vec![],
    };
    let orders = TableMeta {
        schema: "public".to_string(),
        name: "orders".to_string(),
        kind: TableKind::Table,
        columns: vec![
            col("id", "int8", true),
            col("user_id", "int8", false),
            col("total_cents", "int4", false),
        ],
        primary_key: vec!["id".into()],
        foreign_keys: vec![ForeignKey {
            from_column: "user_id".into(),
            to_schema: "public".into(),
            to_table: "users".into(),
            to_column: "id".into(),
        }],
    };
    let audit_events = TableMeta {
        schema: "audit".to_string(),
        name: "events".to_string(),
        kind: TableKind::Table,
        columns: vec![
            col("id", "int8", true),
            col("payload", "jsonb", false),
        ],
        primary_key: vec!["id".into()],
        foreign_keys: vec![],
    };
    SchemaCache::new(DbMeta {
        tables: vec![users, orders, audit_events],
        default_schema: "public".to_string(),
    })
}

fn run(sql: &str, cursor: usize, cache: &SchemaCache) -> Vec<dbtool::sql_complete::CompletionItem> {
    complete(CompletionRequest {
        sql,
        cursor_char: cursor,
        cache,
        dialect: DbKind::Postgres,
    })
}

fn labels(items: &[dbtool::sql_complete::CompletionItem]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

fn has_label_of_kind(
    items: &[dbtool::sql_complete::CompletionItem],
    lbl: &str,
    kind: ItemKind,
) -> bool {
    items.iter().any(|i| i.label == lbl && i.kind == kind)
}

#[test]
fn statement_start_offers_select() {
    let c = fixture();
    let items = run("", 0, &c);
    assert!(has_label_of_kind(&items, "SELECT", ItemKind::Keyword), "labels: {:?}", labels(&items));
}

#[test]
fn after_from_offers_tables() {
    let c = fixture();
    let sql = "SELECT * FROM ";
    let items = run(sql, sql.chars().count(), &c);
    assert!(has_label_of_kind(&items, "users", ItemKind::Table), "labels: {:?}", labels(&items));
    assert!(has_label_of_kind(&items, "orders", ItemKind::Table));
    // qualified form for non-default schema
    assert!(has_label_of_kind(&items, "audit.events", ItemKind::Table));
}

#[test]
fn after_from_filters_by_prefix() {
    let c = fixture();
    let sql = "SELECT * FROM us";
    let items = run(sql, sql.chars().count(), &c);
    // Top candidate should be `users`.
    assert_eq!(items.first().map(|i| i.label.as_str()), Some("users"), "labels: {:?}", labels(&items));
}

#[test]
fn alias_dot_completes_columns() {
    let c = fixture();
    let sql = "SELECT * FROM users u WHERE u.";
    let items = run(sql, sql.chars().count(), &c);
    // We expect columns of users (id, email, name, created_at).
    assert!(has_label_of_kind(&items, "email", ItemKind::Column), "labels: {:?}", labels(&items));
    assert!(has_label_of_kind(&items, "name", ItemKind::Column));
    // No orders columns.
    assert!(!has_label_of_kind(&items, "total_cents", ItemKind::Column));
}

#[test]
fn schema_dot_completes_tables() {
    let c = fixture();
    let sql = "SELECT * FROM audit.";
    let items = run(sql, sql.chars().count(), &c);
    assert!(has_label_of_kind(&items, "events", ItemKind::Table), "labels: {:?}", labels(&items));
}

#[test]
fn where_clause_offers_in_scope_columns() {
    let c = fixture();
    let sql = "SELECT * FROM orders o WHERE ";
    let items = run(sql, sql.chars().count(), &c);
    assert!(has_label_of_kind(&items, "o.total_cents", ItemKind::Column)
         || has_label_of_kind(&items, "total_cents", ItemKind::Column),
        "labels: {:?}", labels(&items));
}

#[test]
fn fk_join_suggestion() {
    let c = fixture();
    let sql = "SELECT * FROM users JOIN orders ";
    let items = run(sql, sql.chars().count(), &c);
    // Expect FK suggestion: ON users.id = orders.user_id
    let fk = items
        .iter()
        .find(|i| i.kind == ItemKind::JoinFk && i.label.contains("ON"));
    assert!(fk.is_some(), "no JoinFk found. labels: {:?}", labels(&items));
    let text = fk.unwrap().label.clone();
    assert!(text.contains("users.id"), "text: {text}");
    assert!(text.contains("orders.user_id"), "text: {text}");
}

#[test]
fn dead_zone_in_string_literal_disables_completion() {
    let c = fixture();
    let sql = "SELECT * FROM users WHERE email = 'he";
    let items = run(sql, sql.chars().count(), &c);
    assert!(items.is_empty(), "expected no items in string, got {:?}", labels(&items));
}

#[test]
fn dead_zone_in_comment_disables_completion() {
    let c = fixture();
    let sql = "SELECT * -- todo: fil";
    let items = run(sql, sql.chars().count(), &c);
    assert!(items.is_empty(), "expected no items in comment, got {:?}", labels(&items));
}

#[test]
fn dot_completion_on_table_name_without_alias() {
    let c = fixture();
    let sql = "SELECT * FROM users WHERE users.";
    let items = run(sql, sql.chars().count(), &c);
    assert!(has_label_of_kind(&items, "email", ItemKind::Column), "labels: {:?}", labels(&items));
}

#[test]
fn cte_scope_carries_name_into_from() {
    let c = fixture();
    // After a CTE declaration, the CTE name should be a valid candidate in FROM.
    let sql = "WITH t AS (SELECT 1 AS x) SELECT * FROM ";
    let items = run(sql, sql.chars().count(), &c);
    assert!(has_label_of_kind(&items, "t", ItemKind::Cte), "labels: {:?}", labels(&items));
}

#[test]
fn case_upper_preference_on_keywords() {
    let c = fixture();
    // Typing upper prefix keeps uppercase keyword suggestion.
    let sql = "SEL";
    let items = run(sql, sql.chars().count(), &c);
    // SELECT should appear uppercase when the user's prefix is upper.
    assert!(has_label_of_kind(&items, "SELECT", ItemKind::Keyword), "labels: {:?}", labels(&items));

    // Lowercase prefix.
    let sql = "sel";
    let items = run(sql, sql.chars().count(), &c);
    assert!(has_label_of_kind(&items, "select", ItemKind::Keyword), "labels: {:?}", labels(&items));
}

#[test]
fn insert_column_list_offers_target_columns() {
    let c = fixture();
    let sql = "INSERT INTO users (";
    let items = run(sql, sql.chars().count(), &c);
    assert!(has_label_of_kind(&items, "email", ItemKind::Column), "labels: {:?}", labels(&items));
    assert!(has_label_of_kind(&items, "name", ItemKind::Column));
}

#[test]
fn columns_offered_when_cursor_is_between_select_and_from() {
    let c = fixture();
    // Cursor at char index 7 — between SELECT and FROM, after replacing `*` with empty.
    let _ = c.default_schema(); // silence unused warning path
    let sql = "SELECT  FROM users";
    let items = run(sql, 7, &c);
    assert!(
        has_label_of_kind(&items, "email", ItemKind::Column)
            || has_label_of_kind(&items, "name", ItemKind::Column),
        "labels: {:?}",
        labels(&items)
    );
}

#[test]
fn columns_offered_when_cursor_in_select_list_with_quoted_from() {
    let c = fixture();
    // Quoted table that exists in the fixture.
    let sql = r#"SELECT  FROM "users""#;
    let items = run(sql, 7, &c);
    assert!(
        has_label_of_kind(&items, "email", ItemKind::Column),
        "labels: {:?}",
        labels(&items)
    );
}

#[test]
fn subquery_scope_does_not_leak_outer_from() {
    let c = fixture();
    // Cursor is inside the inner SELECT's projection list. We want `orders`' columns,
    // NOT `users`' columns from the outer FROM.
    let sql = "SELECT (SELECT  FROM orders) FROM users";
    // cursor is at position after "(SELECT " — char index 15
    let cursor = 15;
    let items = run(sql, cursor, &c);
    assert!(
        has_label_of_kind(&items, "total_cents", ItemKind::Column),
        "expected orders columns in inner scope. labels: {:?}",
        labels(&items)
    );
    // Should NOT see users-specific columns like `email`.
    assert!(
        !has_label_of_kind(&items, "email", ItemKind::Column),
        "users columns leaked into subquery scope: {:?}",
        labels(&items)
    );
}

#[test]
fn unknown_schema_dot_returns_nothing_for_that_schema() {
    let c = fixture();
    let sql = "SELECT * FROM nope.";
    let items = run(sql, sql.chars().count(), &c);
    // "nope" isn't a schema; we should get zero or at worst non-table items (but no tables of nope).
    for it in &items {
        if it.kind == ItemKind::Table {
            panic!("did not expect table item for unknown schema: {:?}", it);
        }
    }
}
