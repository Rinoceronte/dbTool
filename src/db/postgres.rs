use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use sqlx::{Column as _, Row as _, TypeInfo, ValueRef};
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};

use std::collections::HashMap;

use super::{
    ColumnSchema, ConnectParams, DbKind, DbMeta, Driver, ForeignKey, PkValues, ResultSet,
    RowChanges, TableInfo, TableKind, TableMeta, TableSchema, Value,
    structure::{
        CheckInfo, ColumnInfo, DdlOutcome, FkInfo, IdentityKind, IndexInfo, KeyInfo,
        TableStructure,
    },
    types::Column as MetaColumn,
};

pub struct PostgresDriver {
    pool: PgPool,
}

impl PostgresDriver {
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&params.to_url())
            .await
            .context("postgres connect")?;
        Ok(Self { pool })
    }
}

fn pg_value_from_row(row: &PgRow, idx: usize) -> Value {
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
        "BOOL" => try_as!(bool, Value::Bool),
        "INT2" => try_as!(i16, |v| Value::Int(v as i64)),
        "INT4" => try_as!(i32, |v| Value::Int(v as i64)),
        "INT8" => try_as!(i64, Value::Int),
        "FLOAT4" => try_as!(f32, |v| Value::Float(v as f64)),
        "FLOAT8" => try_as!(f64, Value::Float),
        "NUMERIC" => try_as!(String, Value::Text),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => try_as!(String, Value::Text),
        "UUID" => try_as!(uuid::Uuid, |v: uuid::Uuid| Value::Text(v.to_string())),
        "JSON" | "JSONB" => try_as!(serde_json::Value, Value::Json),
        "BYTEA" => try_as!(Vec<u8>, Value::Bytes),
        "TIMESTAMP" | "TIMESTAMPTZ" => {
            if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
                return Value::Timestamp(v.to_rfc3339());
            }
            if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                return Value::Timestamp(v.to_string());
            }
        }
        "DATE" => try_as!(chrono::NaiveDate, |v: chrono::NaiveDate| Value::Timestamp(v.to_string())),
        "TIME" => try_as!(chrono::NaiveTime, |v: chrono::NaiveTime| Value::Timestamp(v.to_string())),
        _ => {}
    }
    // Fallback: try as string
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Value::Text(v);
    }
    Value::Text(format!("<{}>", name))
}

fn result_set_from_pg_rows(rows: Vec<PgRow>) -> ResultSet {
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
        .map(|r| (0..r.columns().len()).map(|i| pg_value_from_row(r, i)).collect())
        .collect();
    ResultSet { columns, rows: data, rows_affected: None }
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Column metadata for a query that returned no rows (sqlx rows carry the
/// metadata, so an empty result loses it). Uses PREPARE; never executes.
async fn columns_via_describe(pool: &PgPool, sql: &str) -> Vec<MetaColumn> {
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

/// `pg_get_constraintdef` returns `CHECK (expr)` — keep just the expression.
fn strip_check_wrapper(def: &str) -> String {
    let t = def.trim();
    if let Some(rest) = t.strip_prefix("CHECK ") {
        let r = rest.trim();
        if r.starts_with('(') && r.ends_with(')') {
            return r[1..r.len() - 1].to_string();
        }
        return r.to_string();
    }
    t.to_string()
}

fn bind_value<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    v: &'q Value,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
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
impl Driver for PostgresDriver {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name NOT IN ('pg_catalog', 'information_schema') \
               AND schema_name NOT LIKE 'pg_toast%' \
               AND schema_name NOT LIKE 'pg_temp%' \
             ORDER BY schema_name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(s,)| s).collect())
    }

    async fn list_tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT table_name, table_type FROM information_schema.tables \
             WHERE table_schema = $1 ORDER BY table_name",
        )
        .bind(schema)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, kind)| TableInfo {
                schema: schema.to_string(),
                name,
                kind: if kind.contains("VIEW") { TableKind::View } else { TableKind::Table },
            })
            .collect())
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableSchema> {
        let cols: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT column_name, data_type, is_nullable, column_default \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let pk_cols: Vec<(String,)> = sqlx::query_as(
            "SELECT kcu.column_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON tc.constraint_name = kcu.constraint_name \
              AND tc.table_schema = kcu.table_schema \
             WHERE tc.constraint_type = 'PRIMARY KEY' \
               AND tc.table_schema = $1 AND tc.table_name = $2 \
             ORDER BY kcu.ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;
        let pk: Vec<String> = pk_cols.into_iter().map(|(c,)| c).collect();
        let pk_set: std::collections::HashSet<&str> = pk.iter().map(|s| s.as_str()).collect();

        let columns = cols
            .into_iter()
            .map(|(name, type_name, is_nullable, _default)| {
                let is_pk = pk_set.contains(name.as_str());
                ColumnSchema {
                    is_primary_key: is_pk,
                    nullable: is_nullable == "YES",
                    name,
                    type_name,
                }
            })
            .collect();

        Ok(TableSchema { columns, primary_key: pk })
    }

    async fn query(&self, sql: &str) -> Result<ResultSet> {
        let trimmed = sql.trim_start().to_ascii_uppercase();
        let is_select = trimmed.starts_with("SELECT") || trimmed.starts_with("WITH") || trimmed.starts_with("SHOW");
        if is_select {
            let rows = sqlx::query(sql).fetch_all(&self.pool).await?;
            let mut rs = result_set_from_pg_rows(rows);
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
            })
        }
    }

    async fn fetch_table_rows(
        &self,
        schema: &str,
        table: &str,
        limit: i64,
        offset: i64,
    ) -> Result<ResultSet> {
        let sql = format!(
            "SELECT * FROM {}.{} LIMIT {} OFFSET {}",
            ident(schema),
            ident(table),
            limit,
            offset
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        let mut rs = result_set_from_pg_rows(rows);
        if rs.columns.is_empty() {
            rs.columns = columns_via_describe(&self.pool, &sql).await;
        }
        Ok(rs)
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
        let mut sql = format!("UPDATE {}.{} SET ", ident(schema), ident(table));
        let mut param_idx = 1usize;
        let set_parts: Vec<String> = changes
            .keys()
            .map(|c| {
                let p = format!("{} = ${}", ident(c), param_idx);
                param_idx += 1;
                p
            })
            .collect();
        sql.push_str(&set_parts.join(", "));
        sql.push_str(" WHERE ");
        let where_parts: Vec<String> = pk
            .keys()
            .map(|c| {
                let p = format!("{} = ${}", ident(c), param_idx);
                param_idx += 1;
                p
            })
            .collect();
        sql.push_str(&where_parts.join(" AND "));

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
        let placeholders: Vec<String> = (1..=values.len()).map(|i| format!("${}", i)).collect();
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
        let mut param_idx = 1usize;
        let where_parts: Vec<String> = pk
            .keys()
            .map(|c| {
                let p = format!("{} = ${}", ident(c), param_idx);
                param_idx += 1;
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
            let mut param_idx = 1usize;
            let set_parts: Vec<String> = changes
                .keys()
                .map(|c| {
                    let p = format!("{} = ${}", ident(c), param_idx);
                    param_idx += 1;
                    p
                })
                .collect();
            let where_parts: Vec<String> = pk
                .keys()
                .map(|c| {
                    let p = format!("{} = ${}", ident(c), param_idx);
                    param_idx += 1;
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
            let mut param_idx = 1usize;
            let where_parts: Vec<String> = pk
                .keys()
                .map(|c| {
                    let p = format!("{} = ${}", ident(c), param_idx);
                    param_idx += 1;
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

    async fn table_ddl(&self, schema: &str, table: &str, kind: TableKind) -> Result<String> {
        if kind == TableKind::View {
            let (def,): (String,) = sqlx::query_as(
                "SELECT pg_catalog.pg_get_viewdef(format('%I.%I', $1::text, $2::text)::regclass, true)",
            )
            .bind(schema)
            .bind(table)
            .fetch_one(&self.pool)
            .await?;
            return Ok(format!(
                "CREATE OR REPLACE VIEW {}.{} AS\n{}\n",
                ident(schema),
                ident(table),
                def.trim_end()
            ));
        }

        // (name, formatted type, not null, default expr, identity, generated)
        let cols: Vec<(String, String, bool, Option<String>, String, String)> = sqlx::query_as(
            "SELECT a.attname, \
                    pg_catalog.format_type(a.atttypid, a.atttypmod), \
                    a.attnotnull, \
                    pg_catalog.pg_get_expr(d.adbin, d.adrelid), \
                    a.attidentity::text, \
                    a.attgenerated::text \
             FROM pg_catalog.pg_attribute a \
             LEFT JOIN pg_catalog.pg_attrdef d \
               ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = format('%I.%I', $1::text, $2::text)::regclass \
               AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let constraints: Vec<(String, String)> = sqlx::query_as(
            "SELECT conname, pg_catalog.pg_get_constraintdef(c.oid, true) \
             FROM pg_catalog.pg_constraint c \
             WHERE c.conrelid = format('%I.%I', $1::text, $2::text)::regclass \
             ORDER BY CASE c.contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 ELSE 3 END, conname",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        // Secondary indexes not backed by a constraint.
        let indexes: Vec<(String,)> = sqlx::query_as(
            "SELECT pg_catalog.pg_get_indexdef(i.indexrelid) \
             FROM pg_catalog.pg_index i \
             JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
             WHERE i.indrelid = format('%I.%I', $1::text, $2::text)::regclass \
               AND NOT i.indisprimary \
               AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint c WHERE c.conindid = i.indexrelid) \
             ORDER BY ic.relname",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let mut parts: Vec<String> = Vec::new();
        for (name, ty, not_null, default, identity, generated) in cols {
            let mut line = format!("    {} {}", ident(&name), ty);
            if generated == "s" {
                if let Some(expr) = &default {
                    line.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED"));
                }
            } else if identity == "a" {
                line.push_str(" GENERATED ALWAYS AS IDENTITY");
            } else if identity == "d" {
                line.push_str(" GENERATED BY DEFAULT AS IDENTITY");
            } else if let Some(expr) = &default {
                line.push_str(&format!(" DEFAULT {expr}"));
            }
            if not_null {
                line.push_str(" NOT NULL");
            }
            parts.push(line);
        }
        for (name, def) in constraints {
            parts.push(format!("    CONSTRAINT {} {}", ident(&name), def));
        }

        let mut ddl = format!(
            "CREATE TABLE {}.{} (\n{}\n);\n",
            ident(schema),
            ident(table),
            parts.join(",\n")
        );
        for (idx,) in indexes {
            ddl.push_str(&format!("\n{idx};\n"));
        }
        Ok(ddl)
    }

    async fn describe_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        // Columns: (name, formatted type, not null, default expr, identity, generated)
        let cols: Vec<(String, String, bool, Option<String>, String, String)> = sqlx::query_as(
            "SELECT a.attname, \
                    pg_catalog.format_type(a.atttypid, a.atttypmod), \
                    a.attnotnull, \
                    pg_catalog.pg_get_expr(d.adbin, d.adrelid), \
                    a.attidentity::text, \
                    a.attgenerated::text \
             FROM pg_catalog.pg_attribute a \
             LEFT JOIN pg_catalog.pg_attrdef d \
               ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = format('%I.%I', $1::text, $2::text)::regclass \
               AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let pk_rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT c.conname, a.attname \
             FROM pg_catalog.pg_constraint c \
             JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) ON true \
             JOIN pg_catalog.pg_attribute a \
               ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
             WHERE c.conrelid = format('%I.%I', $1::text, $2::text)::regclass \
               AND c.contype = 'p' \
             ORDER BY k.ord",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        // FKs with per-constraint column arrays and referential actions.
        let fk_rows: Vec<(String, String, String, String, String, Vec<String>, Vec<String>)> =
            sqlx::query_as(
                "SELECT c.conname, \
                        rns.nspname, \
                        rc.relname, \
                        c.confdeltype::text, \
                        c.confupdtype::text, \
                        (SELECT array_agg(a.attname ORDER BY k.ord) \
                           FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) \
                           JOIN pg_catalog.pg_attribute a \
                             ON a.attrelid = c.conrelid AND a.attnum = k.attnum), \
                        (SELECT array_agg(a.attname ORDER BY k.ord) \
                           FROM unnest(c.confkey) WITH ORDINALITY AS k(attnum, ord) \
                           JOIN pg_catalog.pg_attribute a \
                             ON a.attrelid = c.confrelid AND a.attnum = k.attnum) \
                 FROM pg_catalog.pg_constraint c \
                 JOIN pg_catalog.pg_class rc ON rc.oid = c.confrelid \
                 JOIN pg_catalog.pg_namespace rns ON rns.oid = rc.relnamespace \
                 WHERE c.conrelid = format('%I.%I', $1::text, $2::text)::regclass \
                   AND c.contype = 'f' \
                 ORDER BY c.conname",
            )
            .bind(schema)
            .bind(table)
            .fetch_all(&self.pool)
            .await?;

        // Secondary indexes not backed by any constraint. Expression indexes
        // (attnum 0 entries) are skipped — not editable in the UI.
        let idx_rows: Vec<(String, bool, Option<Vec<String>>, i16)> = sqlx::query_as(
            "SELECT ic.relname, i.indisunique, \
                    (SELECT array_agg(a.attname ORDER BY k.ord) \
                       FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                       JOIN pg_catalog.pg_attribute a \
                         ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                      WHERE k.attnum > 0), \
                    i.indnatts \
             FROM pg_catalog.pg_index i \
             JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
             WHERE i.indrelid = format('%I.%I', $1::text, $2::text)::regclass \
               AND NOT i.indisprimary \
               AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint c WHERE c.conindid = i.indexrelid) \
             ORDER BY ic.relname",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let check_rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT conname, pg_catalog.pg_get_constraintdef(oid, true) \
             FROM pg_catalog.pg_constraint \
             WHERE conrelid = format('%I.%I', $1::text, $2::text)::regclass \
               AND contype = 'c' \
             ORDER BY conname",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&self.pool)
        .await?;

        let columns = cols
            .into_iter()
            .map(|(name, ty, not_null, default, identity, generated)| ColumnInfo {
                name,
                type_name: ty,
                not_null,
                identity: match identity.as_str() {
                    "a" => IdentityKind::Always,
                    "d" => IdentityKind::ByDefault,
                    _ => IdentityKind::None,
                },
                generated: generated == "s",
                default,
                default_constraint: None,
            })
            .collect();

        let primary_key = if pk_rows.is_empty() {
            None
        } else {
            Some(KeyInfo {
                name: Some(pk_rows[0].0.clone()),
                columns: pk_rows.into_iter().map(|(_, c)| c).collect(),
            })
        };

        fn action(code: &str) -> String {
            match code {
                "r" => "RESTRICT",
                "c" => "CASCADE",
                "n" => "SET NULL",
                "d" => "SET DEFAULT",
                _ => "NO ACTION",
            }
            .to_string()
        }

        let foreign_keys = fk_rows
            .into_iter()
            .map(|(name, ref_schema, ref_table, del, upd, cols, ref_cols)| FkInfo {
                name,
                columns: cols,
                ref_schema,
                ref_table,
                ref_columns: ref_cols,
                on_delete: action(&del),
                on_update: action(&upd),
            })
            .collect();

        let indexes = idx_rows
            .into_iter()
            .filter_map(|(name, unique, cols, natts)| {
                let cols = cols?;
                // Expression/partial-key indexes surface fewer names than
                // indnatts — leave those out rather than mis-edit them.
                if cols.len() != natts as usize {
                    return None;
                }
                Some(IndexInfo { name, columns: cols, unique })
            })
            .collect();

        let checks = check_rows
            .into_iter()
            .map(|(name, def)| CheckInfo {
                name,
                expression: strip_check_wrapper(&def),
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
        let default_schema: (String,) =
            sqlx::query_as("SELECT COALESCE(current_schema(), 'public')")
                .fetch_one(&self.pool)
                .await?;
        let default_schema = default_schema.0;

        // All tables/views across user schemas.
        let table_rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT table_schema, table_name, table_type FROM information_schema.tables \
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
               AND table_schema NOT LIKE 'pg_toast%' \
               AND table_schema NOT LIKE 'pg_temp%' \
             ORDER BY table_schema, table_name",
        )
        .fetch_all(&self.pool)
        .await?;

        // All columns for those tables.
        let col_rows: Vec<(String, String, String, String, String, i32)> = sqlx::query_as(
            "SELECT table_schema, table_name, column_name, data_type, is_nullable, ordinal_position \
             FROM information_schema.columns \
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
               AND table_schema NOT LIKE 'pg_toast%' \
               AND table_schema NOT LIKE 'pg_temp%' \
             ORDER BY table_schema, table_name, ordinal_position",
        )
        .fetch_all(&self.pool)
        .await?;

        // Primary keys.
        let pk_rows: Vec<(String, String, String, i32)> = sqlx::query_as(
            "SELECT tc.table_schema, tc.table_name, kcu.column_name, kcu.ordinal_position \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON tc.constraint_name = kcu.constraint_name \
              AND tc.table_schema    = kcu.table_schema \
             WHERE tc.constraint_type = 'PRIMARY KEY' \
               AND tc.table_schema NOT IN ('pg_catalog', 'information_schema') \
             ORDER BY tc.table_schema, tc.table_name, kcu.ordinal_position",
        )
        .fetch_all(&self.pool)
        .await?;

        // Foreign keys.
        let fk_rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT kcu.table_schema, kcu.table_name, kcu.column_name, \
                    ccu.table_schema, ccu.table_name, ccu.column_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON tc.constraint_name = kcu.constraint_name \
              AND tc.table_schema    = kcu.table_schema \
             JOIN information_schema.constraint_column_usage ccu \
               ON ccu.constraint_name = tc.constraint_name \
              AND ccu.table_schema    = tc.table_schema \
             WHERE tc.constraint_type = 'FOREIGN KEY' \
               AND tc.table_schema NOT IN ('pg_catalog', 'information_schema')",
        )
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
        for (schema, name, col, ty, nullable, _pos) in col_rows {
            let key = (schema, name);
            if let Some(t) = by_key.get_mut(&key) {
                t.columns.push(ColumnSchema {
                    name: col,
                    type_name: ty,
                    nullable: nullable == "YES",
                    is_primary_key: false,
                });
            }
        }
        for (schema, name, col, _pos) in pk_rows {
            let key = (schema, name);
            if let Some(t) = by_key.get_mut(&key) {
                t.primary_key.push(col.clone());
                if let Some(c) = t.columns.iter_mut().find(|c| c.name == col) {
                    c.is_primary_key = true;
                }
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
