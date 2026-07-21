use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use sqlx::{Column as _, Row as _, TypeInfo, ValueRef};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};

use std::collections::HashMap;

use super::{
    ColumnSchema, ConnectParams, DbKind, DbMeta, Driver, ForeignKey, PkValues, ResultSet,
    RowChanges, RowsFilter, SchemaObjects, TableInfo, TableKind, TableMeta, TableSchema, Value,
    structure::{
        CheckInfo, ColumnInfo, DdlOutcome, FkInfo, IdentityKind, IndexInfo, KeyInfo,
        TableStructure,
    },
    types::{Column as MetaColumn, RoutineInfo, TriggerInfo},
};

pub struct MySqlDriver {
    pool: MySqlPool,
    default_db: String,
}

/// Statement-by-statement on ONE checked-out connection, so BEGIN…COMMIT
/// spanning the script stays on the same session. One ResultSet per
/// data-producing statement; a script with none yields a single summary.
async fn run_script_on(
    conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>,
    sql: &str,
) -> Result<Vec<ResultSet>> {
    use sqlx::Executor as _;
    let mut affected = 0u64;
    let mut sets: Vec<ResultSet> = Vec::new();
    for stmt in crate::db::split_statements(sql) {
        let head = stmt.trim_start().to_ascii_uppercase();
        let returns_rows = head.starts_with("SELECT")
            || head.starts_with("WITH")
            || head.starts_with("SHOW")
            || head.starts_with("DESCRIBE")
            || head.starts_with("DESC ")
            || head.starts_with("EXPLAIN");
        if returns_rows {
            let rs = fetch_capped(&mut **conn, &stmt).await?;
            if !rs.columns.is_empty() {
                sets.push(rs);
            }
        } else {
            let res = conn.execute(sqlx::query(&stmt)).await?;
            affected += res.rows_affected();
        }
    }
    if sets.is_empty() {
        sets.push(ResultSet {
            columns: vec![],
            rows: vec![],
            rows_affected: Some(affected),
            truncated: false,
        });
    }
    Ok(sets)
}

impl MySqlDriver {
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .connect(&params.to_url())
            .await
            .context("mysql connect")?;
        Ok(Self { pool, default_db: params.database.clone() })
    }
}

fn mysql_value_from_row(row: &MySqlRow, idx: usize) -> Value {
    let raw = row.try_get_raw(idx).ok();
    let Some(raw) = raw else { return Value::Null };
    if raw.is_null() {
        return Value::Null;
    }
    let ti = raw.type_info();
    let name = ti.name();

    macro_rules! try_as {
        ($t:ty, $ctor:expr) => {{
            if let Ok(v) = row.try_get::<$t, _>(idx) {
                return $ctor(v);
            }
        }};
    }

    match name {
        "BOOLEAN" | "TINYINT(1)" => try_as!(bool, Value::Bool),
        "TINYINT" | "TINYINT UNSIGNED" => try_as!(i8, |v| Value::Int(v as i64)),
        "SMALLINT" | "SMALLINT UNSIGNED" => try_as!(i16, |v| Value::Int(v as i64)),
        "INT" | "MEDIUMINT" | "INT UNSIGNED" | "MEDIUMINT UNSIGNED" => try_as!(i32, |v| Value::Int(v as i64)),
        "BIGINT" | "BIGINT UNSIGNED" => try_as!(i64, Value::Int),
        "FLOAT" => try_as!(f32, |v| Value::Float(v as f64)),
        "DOUBLE" => try_as!(f64, Value::Float),
        "DECIMAL" | "NUMERIC" => try_as!(String, Value::Text),
        "JSON" => try_as!(serde_json::Value, Value::Json),
        "BLOB" | "LONGBLOB" | "MEDIUMBLOB" | "TINYBLOB" | "VARBINARY" | "BINARY" => {
            try_as!(Vec<u8>, Value::Bytes)
        }
        "DATETIME" | "TIMESTAMP" => {
            if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                return Value::Timestamp(v.to_string());
            }
            if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
                return Value::Timestamp(v.to_rfc3339());
            }
        }
        "DATE" => try_as!(chrono::NaiveDate, |v: chrono::NaiveDate| Value::Timestamp(v.to_string())),
        "TIME" => try_as!(chrono::NaiveTime, |v: chrono::NaiveTime| Value::Timestamp(v.to_string())),
        _ => {}
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::Text(v);
    }
    Value::Text(format!("<{}>", name))
}

/// Stream a SELECT, stopping row materialization at the configured cap.
async fn fetch_capped<'e, E>(executor: E, sql: &str) -> Result<ResultSet>
where
    E: sqlx::Executor<'e, Database = sqlx::MySql>,
{
    use futures::TryStreamExt as _;
    let cap = crate::db::effective_row_cap();
    let mut stream = executor.fetch(sqlx::query(sql));
    let mut rows: Vec<MySqlRow> = Vec::new();
    let mut truncated = false;
    while let Some(row) = stream.try_next().await? {
        if rows.len() >= cap {
            truncated = true;
            break;
        }
        rows.push(row);
    }
    drop(stream);
    let mut rs = result_set_from_my_rows(rows);
    rs.truncated = truncated;
    Ok(rs)
}

fn result_set_from_my_rows(rows: Vec<MySqlRow>) -> ResultSet {
    let columns: Vec<MetaColumn> = if let Some(first) = rows.first() {
        first
            .columns()
            .iter()
            .map(|c| MetaColumn {
                name: c.name().to_string(),
                type_name: c.type_info().name().to_string(),
            })
            .collect()
    } else {
        vec![]
    };
    let data: Vec<Vec<Value>> = rows
        .iter()
        .map(|r| (0..r.columns().len()).map(|i| mysql_value_from_row(r, i)).collect())
        .collect();
    ResultSet { columns, rows: data, rows_affected: None, truncated: false }
}

fn ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Column metadata for a query that returned no rows (sqlx rows carry the
/// metadata, so an empty result loses it). Uses PREPARE; never executes.
async fn columns_via_describe(pool: &MySqlPool, sql: &str) -> Vec<MetaColumn> {
    use sqlx::Executor;
    match pool.describe(sql).await {
        Ok(d) => d
            .columns()
            .iter()
            .map(|c| MetaColumn {
                name: c.name().to_string(),
                type_name: c.type_info().name().to_string(),
            })
            .collect(),
        Err(_) => vec![],
    }
}

fn bind_value<'q>(
    q: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    v: &'q Value,
) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    match v {
        Value::Null => q.bind(Option::<String>::None),
        Value::Bool(b) => q.bind(*b),
        Value::Int(i) => q.bind(*i),
        Value::Float(f) => q.bind(*f),
        Value::Text(s) => q.bind(s.as_str()),
        Value::Bytes(b) => q.bind(b.as_slice()),
        Value::Json(j) => q.bind(j),
        Value::Timestamp(s) => q.bind(s.as_str()),
    }
}

#[async_trait]
impl Driver for MySqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::MySql
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        // In MySQL, "schema" == "database". Show the connected database only by default;
        // advanced users can see others if they have privileges.
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys') \
             ORDER BY schema_name",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<String> = rows.into_iter().map(|(s,)| s).collect();
        if !out.iter().any(|s| s == &self.default_db) && !self.default_db.is_empty() {
            out.insert(0, self.default_db.clone());
        }
        Ok(out)
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        // MySQL databases are already surfaced as schemas in the tree.
        Ok(Vec::new())
    }

    async fn list_tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT table_name, table_type, NULLIF(table_comment, '') \
             FROM information_schema.tables \
             WHERE table_schema = ? ORDER BY table_name",
        )
        .bind(schema)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, kind, comment)| TableInfo {
                schema: schema.to_string(),
                name,
                kind: if kind.contains("VIEW") { TableKind::View } else { TableKind::Table },
                comment,
            })
            .collect())
    }

    async fn table_comments(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<(Option<String>, Vec<(String, Option<String>)>)> {
        let table_comment: Option<String> = sqlx::query_scalar(
            "SELECT NULLIF(table_comment, '') FROM information_schema.tables \
             WHERE table_schema = ? AND table_name = ?",
        )
        .bind(schema)
        .bind(table)
        .fetch_one(&self.pool)
        .await?;
        let cols: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT column_name, NULLIF(column_comment, '') \
             FROM information_schema.columns \
             WHERE table_schema = ? AND table_name = ? \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;
        Ok((table_comment, cols))
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableSchema> {
        let cols: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT column_name, column_type, is_nullable, column_key \
             FROM information_schema.columns \
             WHERE table_schema = ? AND table_name = ? \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let mut pk: Vec<String> = Vec::new();
        let columns: Vec<ColumnSchema> = cols
            .into_iter()
            .map(|(name, type_name, is_nullable, key)| {
                let is_pk = key == "PRI";
                if is_pk {
                    pk.push(name.clone());
                }
                ColumnSchema {
                    is_primary_key: is_pk,
                    nullable: is_nullable.eq_ignore_ascii_case("YES"),
                    name,
                    type_name,
                }
            })
            .collect();

        Ok(TableSchema { columns, primary_key: pk })
    }

    async fn query(&self, sql: &str) -> Result<ResultSet> {
        // Routine DDL bodies contain ';' — never split them.
        if super::is_multi_statement(sql) && !super::is_routine_ddl(sql) {
            let mut conn = self.pool.acquire().await?;
            return run_script_on(&mut conn, sql).await.map(crate::db::collapse_sets);
        }
        if super::is_routine_ddl(sql) {
            // CREATE FUNCTION/PROCEDURE is not preparable — text protocol.
            let res = sqlx::raw_sql(sql).execute(&self.pool).await?;
            return Ok(ResultSet {
                columns: vec![],
                rows: vec![],
                rows_affected: Some(res.rows_affected()),
                truncated: false,
            });
        }
        let trimmed = sql.trim_start().to_ascii_uppercase();
        let is_select = trimmed.starts_with("SELECT")
            || trimmed.starts_with("WITH")
            || trimmed.starts_with("SHOW")
            || trimmed.starts_with("DESCRIBE")
            || trimmed.starts_with("DESC ")
            || trimmed.starts_with("EXPLAIN");
        if is_select {
            let mut rs = fetch_capped(&self.pool, sql).await?;
            if rs.columns.is_empty() {
                rs.columns = columns_via_describe(&self.pool, sql).await;
            }
            Ok(rs)
        } else {
            let res = sqlx::query(sql).execute(&self.pool).await?;
            Ok(ResultSet {
                columns: vec![],
                rows: vec![],
                rows_affected: Some(res.rows_affected()),
                truncated: false,
            })
        }
    }

    async fn query_script(
        &self,
        sql: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Vec<ResultSet>> {
        use sqlx::Executor as _;
        let mut conn = self.pool.acquire().await?;
        let cid: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(&mut *conn)
            .await?;
        let outcome = {
            let fut = async {
                if crate::db::is_multi_statement(sql) && !crate::db::is_routine_ddl(sql) {
                    run_script_on(&mut conn, sql).await
                } else if crate::db::is_routine_ddl(sql) {
                    let res = conn.execute(sqlx::raw_sql(sql)).await?;
                    Ok(vec![ResultSet {
                        columns: vec![],
                        rows: vec![],
                        rows_affected: Some(res.rows_affected()),
                        truncated: false,
                    }])
                } else {
                    let head = sql.trim_start().to_ascii_uppercase();
                    let returns_rows = head.starts_with("SELECT")
                        || head.starts_with("WITH")
                        || head.starts_with("SHOW")
                        || head.starts_with("DESCRIBE")
                        || head.starts_with("DESC ")
                        || head.starts_with("EXPLAIN");
                    if returns_rows {
                        Ok(vec![fetch_capped(&mut *conn, sql).await?])
                    } else {
                        let res = conn.execute(sqlx::query(sql)).await?;
                        Ok(vec![ResultSet {
                            columns: vec![],
                            rows: vec![],
                            rows_affected: Some(res.rows_affected()),
                            truncated: false,
                        }])
                    }
                }
            };
            tokio::select! {
                r = fut => Some(r),
                _ = cancel.cancelled() => None,
            }
        };
        match outcome {
            Some(mut r) => {
                if let Ok(sets) = &mut r {
                    if let [rs] = sets.as_mut_slice() {
                        if rs.columns.is_empty() && rs.rows_affected.is_none() {
                            rs.columns = columns_via_describe(&self.pool, sql).await;
                        }
                    }
                }
                r
            }
            None => {
                let _ = sqlx::query(&format!("KILL QUERY {cid}"))
                    .execute(&self.pool)
                    .await;
                drop(conn.detach());
                Err(anyhow!("Query cancelled"))
            }
        }
    }

    async fn fetch_table_rows(
        &self,
        schema: &str,
        table: &str,
        limit: i64,
        offset: i64,
        filter: &RowsFilter,
    ) -> Result<ResultSet> {
        let mut sql = format!("SELECT * FROM {}.{}", ident(schema), ident(table));
        if !filter.where_clause.trim().is_empty() {
            sql.push_str(&format!(" WHERE {}", filter.where_clause.trim()));
        }
        if let Some(col) = &filter.order_col {
            sql.push_str(&format!(
                " ORDER BY {} {}",
                ident(col),
                if filter.order_desc { "DESC" } else { "ASC" }
            ));
        }
        sql.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        let mut rs = result_set_from_my_rows(rows);
        if rs.columns.is_empty() {
            rs.columns = columns_via_describe(&self.pool, &sql).await;
        }
        Ok(rs)
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<SchemaObjects> {
        let routines: Vec<(String, String)> = sqlx::query_as(
            "SELECT routine_name, LOWER(routine_type)
             FROM information_schema.routines
             WHERE routine_schema = ? ORDER BY routine_name",
        )
        .bind(schema)
        .fetch_all(&self.pool)
        .await?;
        let routines = routines
            .into_iter()
            .map(|(name, kind)| RoutineInfo { name, kind, detail: String::new() })
            .collect();

        let triggers: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT trigger_name, event_object_table, action_timing, event_manipulation
             FROM information_schema.triggers
             WHERE trigger_schema = ? ORDER BY trigger_name",
        )
        .bind(schema)
        .fetch_all(&self.pool)
        .await?;
        let triggers = triggers
            .into_iter()
            .map(|(name, table, timing, event)| TriggerInfo {
                name,
                table,
                detail: format!("{timing} {event}"),
            })
            .collect();

        // MySQL has no sequences; enums live inline in column types.
        Ok(SchemaObjects {
            routines,
            sequences: Vec::new(),
            enums: Vec::new(),
            triggers,
        })
    }

    async fn routine_ddl(&self, schema: &str, name: &str, kind: &str) -> Result<String> {
        let stmt = if kind.eq_ignore_ascii_case("procedure") {
            format!("SHOW CREATE PROCEDURE {}.{}", ident(schema), ident(name))
        } else {
            format!("SHOW CREATE FUNCTION {}.{}", ident(schema), ident(name))
        };
        let rs = self.query(&stmt).await?;
        // Result has a "Create Function"/"Create Procedure" column.
        let col = rs
            .columns
            .iter()
            .position(|c| c.name.to_ascii_lowercase().starts_with("create"))
            .ok_or_else(|| anyhow!("unexpected SHOW CREATE output"))?;
        rs.rows
            .first()
            .and_then(|r| r.get(col))
            .map(|v| v.display())
            .ok_or_else(|| anyhow!("no definition found for {schema}.{name}"))
    }

    async fn update_row(
        &self,
        schema: &str,
        table: &str,
        pk: &PkValues,
        changes: &RowChanges,
    ) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        if pk.is_empty() {
            return Err(anyhow!("update requires a primary key"));
        }
        let set_parts: Vec<String> = changes.keys().map(|c| format!("{} = ?", ident(c))).collect();
        let where_parts: Vec<String> = pk.keys().map(|c| format!("{} = ?", ident(c))).collect();
        let sql = format!(
            "UPDATE {}.{} SET {} WHERE {}",
            ident(schema),
            ident(table),
            set_parts.join(", "),
            where_parts.join(" AND ")
        );
        let mut q = sqlx::query(&sql);
        for v in changes.values() {
            q = bind_value(q, v);
        }
        for v in pk.values() {
            q = bind_value(q, v);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    async fn insert_row(
        &self,
        schema: &str,
        table: &str,
        values: &RowChanges,
    ) -> Result<()> {
        if values.is_empty() {
            return Err(anyhow!("insert requires at least one value"));
        }
        let cols: Vec<String> = values.keys().map(|c| ident(c)).collect();
        let placeholders: Vec<&str> = values.keys().map(|_| "?").collect();
        let sql = format!(
            "INSERT INTO {}.{} ({}) VALUES ({})",
            ident(schema),
            ident(table),
            cols.join(", "),
            placeholders.join(", ")
        );
        let mut q = sqlx::query(&sql);
        for v in values.values() {
            q = bind_value(q, v);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    async fn delete_row(
        &self,
        schema: &str,
        table: &str,
        pk: &PkValues,
    ) -> Result<()> {
        if pk.is_empty() {
            return Err(anyhow!("delete requires a primary key"));
        }
        let where_parts: Vec<String> = pk.keys().map(|c| format!("{} = ?", ident(c))).collect();
        let sql = format!(
            "DELETE FROM {}.{} WHERE {}",
            ident(schema),
            ident(table),
            where_parts.join(" AND ")
        );
        let mut q = sqlx::query(&sql);
        for v in pk.values() {
            q = bind_value(q, v);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    async fn apply_changes(
        &self,
        schema: &str,
        table: &str,
        updates: &[(PkValues, RowChanges)],
        deletes: &[PkValues],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (pk, changes) in updates {
            if changes.is_empty() {
                continue;
            }
            if pk.is_empty() {
                return Err(anyhow!("update requires a primary key"));
            }
            let set_parts: Vec<String> =
                changes.keys().map(|c| format!("{} = ?", ident(c))).collect();
            let where_parts: Vec<String> =
                pk.keys().map(|c| format!("{} = ?", ident(c))).collect();
            let sql = format!(
                "UPDATE {}.{} SET {} WHERE {}",
                ident(schema),
                ident(table),
                set_parts.join(", "),
                where_parts.join(" AND ")
            );
            let mut q = sqlx::query(&sql);
            for v in changes.values() {
                q = bind_value(q, v);
            }
            for v in pk.values() {
                q = bind_value(q, v);
            }
            q.execute(&mut *tx).await?;
        }
        for pk in deletes {
            if pk.is_empty() {
                return Err(anyhow!("delete requires a primary key"));
            }
            let where_parts: Vec<String> =
                pk.keys().map(|c| format!("{} = ?", ident(c))).collect();
            let sql = format!(
                "DELETE FROM {}.{} WHERE {}",
                ident(schema),
                ident(table),
                where_parts.join(" AND ")
            );
            let mut q = sqlx::query(&sql);
            for v in pk.values() {
                q = bind_value(q, v);
            }
            q.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn table_ddl(&self, schema: &str, table: &str, kind: TableKind) -> Result<String> {
        // SHOW CREATE TABLE works for views too; the statement is always column 1.
        let stmt = match kind {
            TableKind::Table => "TABLE",
            TableKind::View => "VIEW",
        };
        let sql = format!("SHOW CREATE {} {}.{}", stmt, ident(schema), ident(table));
        let row = sqlx::query(&sql).fetch_one(&self.pool).await?;
        let ddl: String = row.try_get(1).context("read SHOW CREATE output")?;
        Ok(format!("{ddl};\n"))
    }

    async fn describe_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        let cols: Vec<(String, String, String, Option<String>, String)> = sqlx::query_as(
            "SELECT column_name, column_type, is_nullable, column_default, extra \
             FROM information_schema.columns \
             WHERE table_schema = ? AND table_name = ? \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let pk_rows: Vec<(String,)> = sqlx::query_as(
            "SELECT column_name FROM information_schema.key_column_usage \
             WHERE table_schema = ? AND table_name = ? AND constraint_name = 'PRIMARY' \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let fk_rows: Vec<(String, String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT kcu.constraint_name, kcu.column_name, \
                    kcu.referenced_table_schema, kcu.referenced_table_name, \
                    kcu.referenced_column_name, rc.delete_rule, rc.update_rule \
             FROM information_schema.key_column_usage kcu \
             JOIN information_schema.referential_constraints rc \
               ON rc.constraint_schema = kcu.table_schema \
              AND rc.constraint_name = kcu.constraint_name \
             WHERE kcu.table_schema = ? AND kcu.table_name = ? \
               AND kcu.referenced_table_name IS NOT NULL \
             ORDER BY kcu.constraint_name, kcu.ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let idx_rows: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT index_name, non_unique, column_name \
             FROM information_schema.statistics \
             WHERE table_schema = ? AND table_name = ? AND index_name <> 'PRIMARY' \
             ORDER BY index_name, seq_in_index",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        // check_constraints only exists on MySQL 8.0.16+ / MariaDB 10.2+;
        // treat failure as "no checks".
        let check_rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT tc.constraint_name, cc.check_clause \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.check_constraints cc \
               ON cc.constraint_schema = tc.table_schema \
              AND cc.constraint_name = tc.constraint_name \
             WHERE tc.table_schema = ? AND tc.table_name = ? \
               AND tc.constraint_type = 'CHECK'",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let columns = cols
            .into_iter()
            .map(|(name, ty, nullable, default, extra)| {
                let extra_l = extra.to_ascii_lowercase();
                ColumnInfo {
                    name,
                    type_name: ty,
                    not_null: !nullable.eq_ignore_ascii_case("YES"),
                    default,
                    identity: if extra_l.contains("auto_increment") {
                        IdentityKind::AutoIncrement
                    } else {
                        IdentityKind::None
                    },
                    generated: extra_l.contains("generated"),
                    default_constraint: None,
                }
            })
            .collect();

        let primary_key = if pk_rows.is_empty() {
            None
        } else {
            Some(KeyInfo {
                name: None, // MySQL PKs are always named PRIMARY
                columns: pk_rows.into_iter().map(|(c,)| c).collect(),
            })
        };

        let mut foreign_keys: Vec<FkInfo> = Vec::new();
        for (name, col, ref_schema, ref_table, ref_col, del, upd) in fk_rows {
            match foreign_keys.last_mut() {
                Some(last) if last.name == name => {
                    last.columns.push(col);
                    last.ref_columns.push(ref_col);
                }
                _ => foreign_keys.push(FkInfo {
                    name,
                    columns: vec![col],
                    ref_schema,
                    ref_table,
                    ref_columns: vec![ref_col],
                    on_delete: del,
                    on_update: upd,
                }),
            }
        }
        // FK-backing indexes show up in statistics under the FK name; hide
        // them from the editable index list to avoid double-managing them.
        let fk_names: std::collections::HashSet<&str> =
            foreign_keys.iter().map(|f| f.name.as_str()).collect();

        let mut indexes: Vec<IndexInfo> = Vec::new();
        for (name, non_unique, col) in idx_rows {
            if fk_names.contains(name.as_str()) {
                continue;
            }
            match indexes.last_mut() {
                Some(last) if last.name == name => last.columns.push(col),
                _ => indexes.push(IndexInfo { name, columns: vec![col], unique: non_unique == 0 }),
            }
        }

        let checks = check_rows
            .into_iter()
            .map(|(name, clause)| {
                let t = clause.trim();
                let expr = if t.starts_with('(') && t.ends_with(')') {
                    t[1..t.len() - 1].to_string()
                } else {
                    t.to_string()
                };
                CheckInfo { name, expression: expr }
            })
            .collect();

        Ok(TableStructure {
            schema: schema.to_string(),
            name: table.to_string(),
            columns,
            primary_key,
            foreign_keys,
            indexes,
            checks,
        })
    }

    async fn apply_ddl(&self, statements: &[String]) -> DdlOutcome {
        // MySQL DDL is not transactional — run sequentially, stop at the
        // first failure and report how far we got.
        for (i, stmt) in statements.iter().enumerate() {
            if let Err(e) = sqlx::query(stmt).execute(&self.pool).await {
                return DdlOutcome {
                    applied: i,
                    error: Some(format!("statement {} failed: {e:#}", i + 1)),
                };
            }
        }
        DdlOutcome { applied: statements.len(), error: None }
    }

    async fn fetch_db_meta(&self) -> Result<DbMeta> {
        // MySQL "schema" == "database". Limit to connected DB to keep the fetch small
        // and avoid cross-db permission issues.
        let default_schema = self.default_db.clone();

        let table_rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT table_schema, table_name, table_type FROM information_schema.tables \
             WHERE table_schema = ? \
             ORDER BY table_schema, table_name",
        )
        .bind(&default_schema)
        .fetch_all(&self.pool)
        .await?;

        let col_rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT table_schema, table_name, column_name, column_type, is_nullable, column_key \
             FROM information_schema.columns \
             WHERE table_schema = ? \
             ORDER BY table_schema, table_name, ordinal_position",
        )
        .bind(&default_schema)
        .fetch_all(&self.pool)
        .await?;

        // FK: join key_column_usage with referential_constraints to get referenced cols.
        let fk_rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT kcu.table_schema, kcu.table_name, kcu.column_name, \
                    kcu.referenced_table_schema, kcu.referenced_table_name, kcu.referenced_column_name \
             FROM information_schema.key_column_usage kcu \
             WHERE kcu.table_schema = ? \
               AND kcu.referenced_table_name IS NOT NULL",
        )
        .bind(&default_schema)
        .fetch_all(&self.pool)
        .await?;

        type Key = (String, String);
        let mut by_key: HashMap<Key, TableMeta> = HashMap::new();
        for (schema, name, kind) in table_rows {
            let key = (schema.clone(), name.clone());
            by_key.insert(
                key,
                TableMeta {
                    schema,
                    name,
                    kind: if kind.contains("VIEW") { TableKind::View } else { TableKind::Table },
                    columns: Vec::new(),
                    primary_key: Vec::new(),
                    foreign_keys: Vec::new(),
                },
            );
        }
        for (schema, name, col, ty, nullable, col_key) in col_rows {
            let key = (schema, name);
            if let Some(t) = by_key.get_mut(&key) {
                let is_pk = col_key == "PRI";
                if is_pk {
                    t.primary_key.push(col.clone());
                }
                t.columns.push(ColumnSchema {
                    name: col,
                    type_name: ty,
                    nullable: nullable.eq_ignore_ascii_case("YES"),
                    is_primary_key: is_pk,
                });
            }
        }
        for (from_schema, from_table, from_col, to_schema, to_table, to_col) in fk_rows {
            let key = (from_schema, from_table);
            if let Some(t) = by_key.get_mut(&key) {
                t.foreign_keys.push(ForeignKey {
                    from_column: from_col,
                    to_schema,
                    to_table,
                    to_column: to_col,
                });
            }
        }

        let mut tables: Vec<TableMeta> = by_key.into_values().collect();
        tables.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
        Ok(DbMeta { tables, default_schema })
    }
}
