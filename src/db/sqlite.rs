use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Column as _, Row as _, TypeInfo, ValueRef};

use super::{
    ColumnSchema, ConnectParams, DbKind, DbMeta, Driver, ForeignKey, PkValues, ResultSet,
    RowChanges, RowsFilter, SchemaObjects, TableInfo, TableKind, TableMeta, TableSchema, Value,
    structure::{
        ColumnInfo, DdlOutcome, FkInfo, IdentityKind, IndexInfo, KeyInfo, TableStructure,
    },
    types::{Column as MetaColumn, TriggerInfo},
};

pub struct SqliteDriver {
    pool: SqlitePool,
}

impl SqliteDriver {
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let path = params.database.trim();
        if path.is_empty() {
            return Err(anyhow!("SQLite needs a database file path"));
        }
        let expanded = match path.strip_prefix("~/") {
            Some(rest) => dirs::home_dir()
                .map(|h| h.join(rest).to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string()),
            None => path.to_string(),
        };
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(&format!("sqlite://{expanded}?mode=rwc"))
            .await
            .context("sqlite connect")?;
        // Enforce FKs like every other engine does out of the box.
        sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await?;
        Ok(Self { pool })
    }

    async fn run_on_conn(
        &self,
        conn: &mut sqlx::SqliteConnection,
        sql: &str,
    ) -> Result<Vec<ResultSet>> {
        let cap = super::effective_row_cap();
        if super::is_multi_statement(sql) {
            use futures::TryStreamExt as _;
            let mut stream = sqlx::raw_sql(sql).fetch_many(&mut *conn);
            let mut affected = 0u64;
            let mut current: Vec<SqliteRow> = Vec::new();
            let mut cur_truncated = false;
            let mut sets: Vec<ResultSet> = Vec::new();
            while let Some(item) = stream.try_next().await? {
                match item {
                    sqlx::Either::Left(done) => {
                        affected += done.rows_affected();
                        if !current.is_empty() {
                            let mut rs = result_set_from_rows(std::mem::take(&mut current));
                            rs.truncated = std::mem::take(&mut cur_truncated);
                            sets.push(rs);
                        }
                    }
                    sqlx::Either::Right(row) => {
                        if current.len() < cap {
                            current.push(row);
                        } else {
                            cur_truncated = true;
                        }
                    }
                }
            }
            if !current.is_empty() {
                let mut rs = result_set_from_rows(current);
                rs.truncated = cur_truncated;
                sets.push(rs);
            }
            if sets.is_empty() {
                sets.push(ResultSet {
                    columns: vec![],
                    rows: vec![],
                    rows_affected: Some(affected),
                    truncated: false,
                });
            }
            return Ok(sets);
        }
        let trimmed = sql.trim_start().to_ascii_uppercase();
        let is_select = trimmed.starts_with("SELECT")
            || trimmed.starts_with("WITH")
            || trimmed.starts_with("PRAGMA")
            || trimmed.starts_with("EXPLAIN");
        if is_select {
            use futures::TryStreamExt as _;
            let mut stream = sqlx::query(sql).fetch(&mut *conn);
            let mut rows: Vec<SqliteRow> = Vec::new();
            let mut truncated = false;
            while let Some(row) = stream.try_next().await? {
                if rows.len() >= cap {
                    truncated = true;
                    break;
                }
                rows.push(row);
            }
            drop(stream);
            let mut rs = result_set_from_rows(rows);
            rs.truncated = truncated;
            Ok(vec![rs])
        } else {
            let res = sqlx::query(sql).execute(&mut *conn).await?;
            Ok(vec![ResultSet {
                columns: vec![],
                rows: vec![],
                rows_affected: Some(res.rows_affected()),
                truncated: false,
            }])
        }
    }
}

fn value_from_row(row: &SqliteRow, idx: usize) -> Value {
    let raw = row.try_get_raw(idx).ok();
    let Some(raw) = raw else { return Value::Null };
    if raw.is_null() {
        return Value::Null;
    }
    let name = raw.type_info().name().to_ascii_uppercase();
    macro_rules! try_as {
        ($t:ty, $ctor:expr) => {{
            if let Ok(v) = row.try_get::<$t, _>(idx) {
                return $ctor(v);
            }
        }};
    }
    match name.as_str() {
        "BOOLEAN" => try_as!(bool, Value::Bool),
        "INTEGER" | "INT" | "INT4" | "INT8" | "BIGINT" => try_as!(i64, Value::Int),
        "REAL" | "FLOAT" | "DOUBLE" | "NUMERIC" => try_as!(f64, Value::Float),
        "BLOB" => try_as!(Vec<u8>, Value::Bytes),
        "DATETIME" | "DATE" | "TIME" => try_as!(String, Value::Timestamp),
        _ => {}
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::Text(v);
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return Value::Int(v);
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return Value::Float(v);
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return Value::Bytes(v);
    }
    Value::Text(format!("<{name}>"))
}

fn result_set_from_rows(rows: Vec<SqliteRow>) -> ResultSet {
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
        .map(|r| (0..r.columns().len()).map(|i| value_from_row(r, i)).collect())
        .collect();
    ResultSet { columns, rows: data, rows_affected: None, truncated: false }
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn bind_value<'q>(
    q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    v: &'q Value,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match v {
        Value::Null => q.bind(Option::<String>::None),
        Value::Bool(b) => q.bind(*b),
        Value::Int(i) => q.bind(*i),
        Value::Float(f) => q.bind(*f),
        Value::Text(s) => q.bind(s.as_str()),
        Value::Bytes(b) => q.bind(b.as_slice()),
        Value::Json(j) => q.bind(j.to_string()),
        Value::Timestamp(s) => q.bind(s.as_str()),
    }
}

#[async_trait]
impl Driver for SqliteDriver {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        // main + any ATTACHed databases; temp omitted unless in use.
        let rows: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT seq, name, file FROM pragma_database_list")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(_, name, _)| name)
            .filter(|n| n != "temp")
            .collect())
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }

    async fn list_tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
            ident(schema)
        );
        let rows: Vec<(String, String)> = sqlx::query_as(&sql).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|(name, kind)| TableInfo {
                schema: schema.to_string(),
                name,
                kind: if kind == "view" { TableKind::View } else { TableKind::Table },
                comment: None,
            })
            .collect())
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<SchemaObjects> {
        let sql = format!(
            "SELECT name, tbl_name FROM {}.sqlite_master WHERE type = 'trigger' ORDER BY name",
            ident(schema)
        );
        let rows: Vec<(String, String)> = sqlx::query_as(&sql).fetch_all(&self.pool).await?;
        Ok(SchemaObjects {
            routines: vec![],
            sequences: vec![],
            enums: vec![],
            triggers: rows
                .into_iter()
                .map(|(name, table)| TriggerInfo { name, table, detail: String::new() })
                .collect(),
        })
    }

    async fn routine_ddl(&self, schema: &str, name: &str, _kind: &str) -> Result<String> {
        let sql = format!(
            "SELECT sql FROM {}.sqlite_master WHERE name = $1 LIMIT 1",
            ident(schema)
        );
        let row: Option<(Option<String>,)> =
            sqlx::query_as(&sql).bind(name).fetch_optional(&self.pool).await?;
        row.and_then(|(s,)| s)
            .ok_or_else(|| anyhow!("no definition found for {schema}.{name}"))
    }

    async fn table_comments(
        &self,
        _schema: &str,
        _table: &str,
    ) -> Result<(Option<String>, Vec<(String, Option<String>)>)> {
        Err(anyhow!("SQLite does not support table or column comments"))
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableSchema> {
        let cols: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT name, type, \"notnull\", pk FROM pragma_table_info($1, $2) ORDER BY cid",
        )
        .bind(table)
        .bind(schema)
        .fetch_all(&self.pool)
        .await?;
        if cols.is_empty() {
            return Err(anyhow!("no such table: {schema}.{table}"));
        }
        // `pk` is the 1-based position of the column in the primary key.
        let mut pk_cols: Vec<(i64, String)> = cols
            .iter()
            .filter(|(_, _, _, pk)| *pk > 0)
            .map(|(name, _, _, pk)| (*pk, name.clone()))
            .collect();
        pk_cols.sort();
        let primary_key: Vec<String> = pk_cols.into_iter().map(|(_, c)| c).collect();
        let columns = cols
            .into_iter()
            .map(|(name, type_name, notnull, pk)| ColumnSchema {
                is_primary_key: pk > 0,
                nullable: notnull == 0,
                name,
                type_name: if type_name.is_empty() { "ANY".into() } else { type_name },
            })
            .collect();
        Ok(TableSchema { columns, primary_key })
    }

    async fn query(&self, sql: &str) -> Result<ResultSet> {
        let mut conn = self.pool.acquire().await?;
        let sets = self.run_on_conn(&mut conn, sql).await?;
        Ok(super::collapse_sets(sets))
    }

    async fn query_script(
        &self,
        sql: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Vec<ResultSet>> {
        let mut conn = self.pool.acquire().await?;
        // No server to cancel on — dropping the future stops local stepping.
        let fut = self.run_on_conn(&mut conn, sql);
        tokio::select! {
            r = fut => r,
            _ = cancel.cancelled() => {
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
        Ok(result_set_from_rows(rows))
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
        let mut idx = 1usize;
        let set_parts: Vec<String> = changes
            .keys()
            .map(|c| {
                let p = format!("{} = ${idx}", ident(c));
                idx += 1;
                p
            })
            .collect();
        let where_parts: Vec<String> = pk
            .keys()
            .map(|c| {
                let p = format!("{} = ${idx}", ident(c));
                idx += 1;
                p
            })
            .collect();
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

    async fn insert_row(&self, schema: &str, table: &str, values: &RowChanges) -> Result<()> {
        if values.is_empty() {
            return Err(anyhow!("insert requires at least one value"));
        }
        let cols: Vec<String> = values.keys().map(|c| ident(c)).collect();
        let placeholders: Vec<String> = (1..=values.len()).map(|i| format!("${i}")).collect();
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

    async fn delete_row(&self, schema: &str, table: &str, pk: &PkValues) -> Result<()> {
        if pk.is_empty() {
            return Err(anyhow!("delete requires a primary key"));
        }
        let mut idx = 1usize;
        let where_parts: Vec<String> = pk
            .keys()
            .map(|c| {
                let p = format!("{} = ${idx}", ident(c));
                idx += 1;
                p
            })
            .collect();
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
            let mut idx = 1usize;
            let set_parts: Vec<String> = changes
                .keys()
                .map(|c| {
                    let p = format!("{} = ${idx}", ident(c));
                    idx += 1;
                    p
                })
                .collect();
            let where_parts: Vec<String> = pk
                .keys()
                .map(|c| {
                    let p = format!("{} = ${idx}", ident(c));
                    idx += 1;
                    p
                })
                .collect();
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
            let mut idx = 1usize;
            let where_parts: Vec<String> = pk
                .keys()
                .map(|c| {
                    let p = format!("{} = ${idx}", ident(c));
                    idx += 1;
                    p
                })
                .collect();
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

    async fn table_ddl(&self, schema: &str, table: &str, _kind: TableKind) -> Result<String> {
        let master = format!("{}.sqlite_master", ident(schema));
        let sql = format!("SELECT sql FROM {master} WHERE name = $1 AND sql IS NOT NULL LIMIT 1");
        let row: Option<(String,)> =
            sqlx::query_as(&sql).bind(table).fetch_optional(&self.pool).await?;
        let mut ddl = row
            .map(|(s,)| format!("{};\n", s.trim_end().trim_end_matches(';')))
            .ok_or_else(|| anyhow!("no definition found for {schema}.{table}"))?;
        // Named (non-autoindex) secondary indexes keep their own DDL.
        let idx_sql = format!(
            "SELECT sql FROM {master} WHERE type = 'index' AND tbl_name = $1 AND sql IS NOT NULL ORDER BY name"
        );
        let idx: Vec<(String,)> = sqlx::query_as(&idx_sql).bind(table).fetch_all(&self.pool).await?;
        for (s,) in idx {
            ddl.push_str(&format!("\n{};\n", s.trim_end().trim_end_matches(';')));
        }
        Ok(ddl)
    }

    async fn describe_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        let cols: Vec<(String, String, i64, Option<String>, i64)> = sqlx::query_as(
            "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info($1, $2) ORDER BY cid",
        )
        .bind(table)
        .bind(schema)
        .fetch_all(&self.pool)
        .await?;
        if cols.is_empty() {
            return Err(anyhow!("no such table: {schema}.{table}"));
        }

        // AUTOINCREMENT is only visible in the original CREATE TABLE text.
        let create_sql: Option<(Option<String>,)> = sqlx::query_as(&format!(
            "SELECT sql FROM {}.sqlite_master WHERE name = $1 LIMIT 1",
            ident(schema)
        ))
        .bind(table)
        .fetch_optional(&self.pool)
        .await?;
        let has_autoinc = create_sql
            .and_then(|(s,)| s)
            .map(|s| s.to_ascii_uppercase().contains("AUTOINCREMENT"))
            .unwrap_or(false);

        let mut pk_cols: Vec<(i64, String)> = cols
            .iter()
            .filter(|(_, _, _, _, pk)| *pk > 0)
            .map(|(name, _, _, _, pk)| (*pk, name.clone()))
            .collect();
        pk_cols.sort();
        let pk_names: Vec<String> = pk_cols.into_iter().map(|(_, c)| c).collect();

        let columns = cols
            .into_iter()
            .map(|(name, ty, notnull, dflt, pk)| ColumnInfo {
                identity: if has_autoinc && pk > 0 {
                    IdentityKind::AutoIncrement
                } else {
                    IdentityKind::None
                },
                name,
                type_name: if ty.is_empty() { "ANY".into() } else { ty },
                not_null: notnull != 0,
                generated: false,
                default: dflt,
                default_constraint: None,
            })
            .collect();

        // FKs: `id` groups multi-column constraints; `seq` orders columns.
        let fk_rows: Vec<(i64, i64, String, String, Option<String>, String, String)> =
            sqlx::query_as(
                "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete \
                 FROM pragma_foreign_key_list($1, $2) ORDER BY id, seq",
            )
            .bind(table)
            .bind(schema)
            .fetch_all(&self.pool)
            .await?;
        let mut foreign_keys: Vec<FkInfo> = Vec::new();
        let mut last_id = None;
        for (id, _, ref_table, from, to, on_update, on_delete) in fk_rows {
            if last_id != Some(id) {
                foreign_keys.push(FkInfo {
                    name: format!("fk_{id}"),
                    columns: vec![],
                    ref_schema: schema.to_string(),
                    ref_table,
                    ref_columns: vec![],
                    on_delete,
                    on_update,
                });
                last_id = Some(id);
            }
            let fk = foreign_keys.last_mut().unwrap();
            fk.columns.push(from);
            // `to` is NULL when the FK references the target's implicit PK.
            if let Some(to) = to {
                fk.ref_columns.push(to);
            }
        }

        // Secondary indexes created with CREATE INDEX ("c" origin).
        let idx_list: Vec<(i64, String, i64, String, i64)> = sqlx::query_as(
            "SELECT seq, name, \"unique\", origin, partial FROM pragma_index_list($1, $2)",
        )
        .bind(table)
        .bind(schema)
        .fetch_all(&self.pool)
        .await?;
        let mut indexes = Vec::new();
        for (_, name, unique, origin, partial) in idx_list {
            if origin != "c" || partial != 0 {
                continue;
            }
            let cols: Vec<(i64, Option<String>)> = sqlx::query_as(
                "SELECT seqno, name FROM pragma_index_info($1, $2) ORDER BY seqno",
            )
            .bind(&name)
            .bind(schema)
            .fetch_all(&self.pool)
            .await?;
            // Expression index members have NULL names — skip those indexes.
            let names: Option<Vec<String>> = cols.into_iter().map(|(_, n)| n).collect();
            if let Some(columns) = names {
                indexes.push(IndexInfo { name, columns, unique: unique != 0 });
            }
        }

        Ok(TableStructure {
            schema: schema.to_string(),
            name: table.to_string(),
            columns,
            primary_key: if pk_names.is_empty() {
                None
            } else {
                Some(KeyInfo { name: None, columns: pk_names })
            },
            foreign_keys,
            indexes,
            checks: vec![],
        })
    }

    async fn apply_ddl(&self, statements: &[String]) -> DdlOutcome {
        let mut tx = match self.pool.begin().await {
            Ok(t) => t,
            Err(e) => return DdlOutcome { applied: 0, error: Some(format!("{e:#}")) },
        };
        for (i, stmt) in statements.iter().enumerate() {
            if let Err(e) = sqlx::query(stmt).execute(&mut *tx).await {
                let _ = tx.rollback().await;
                return DdlOutcome {
                    applied: 0,
                    error: Some(format!("statement {} failed (rolled back): {e:#}", i + 1)),
                };
            }
        }
        match tx.commit().await {
            Ok(()) => DdlOutcome { applied: statements.len(), error: None },
            Err(e) => DdlOutcome { applied: 0, error: Some(format!("commit failed: {e:#}")) },
        }
    }

    async fn fetch_db_meta(&self) -> Result<DbMeta> {
        let mut tables: Vec<TableMeta> = Vec::new();
        for schema in self.list_schemas().await? {
            for info in self.list_tables(&schema).await? {
                let ts = match self.describe_table(&schema, &info.name).await {
                    Ok(ts) => ts,
                    Err(_) => continue,
                };
                let fk_rows: Vec<(i64, i64, String, String, Option<String>)> = sqlx::query_as(
                    "SELECT id, seq, \"table\", \"from\", \"to\" \
                     FROM pragma_foreign_key_list($1, $2) ORDER BY id, seq",
                )
                .bind(&info.name)
                .bind(&schema)
                .fetch_all(&self.pool)
                .await
                .unwrap_or_default();
                let foreign_keys = fk_rows
                    .into_iter()
                    .filter_map(|(_, _, ref_table, from, to)| {
                        Some(ForeignKey {
                            from_column: from,
                            to_schema: schema.clone(),
                            to_table: ref_table,
                            to_column: to?,
                        })
                    })
                    .collect();
                tables.push(TableMeta {
                    schema: schema.clone(),
                    name: info.name,
                    kind: info.kind,
                    columns: ts.columns,
                    primary_key: ts.primary_key,
                    foreign_keys,
                });
            }
        }
        Ok(DbMeta { tables, default_schema: "main".to_string() })
    }
}
