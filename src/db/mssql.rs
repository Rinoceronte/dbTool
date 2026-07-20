use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, ToSql};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

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

type MsClient = Client<Compat<TcpStream>>;

pub struct MsSqlDriver {
    client: Mutex<MsClient>,
}

impl MsSqlDriver {
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let mut config = Config::new();
        config.host(&params.host);
        config.port(params.port);
        if !params.database.is_empty() {
            config.database(&params.database);
        }
        config.authentication(AuthMethod::sql_server(&params.username, &params.password));
        config.encryption(if params.require_ssl {
            EncryptionLevel::Required
        } else {
            EncryptionLevel::Off
        });
        config.trust_cert();

        let tcp = TcpStream::connect(config.get_addr())
            .await
            .context("mssql tcp connect")?;
        tcp.set_nodelay(true)?;
        let client = Client::connect(config, tcp.compat_write())
            .await
            .context("mssql connect")?;
        Ok(Self { client: Mutex::new(client) })
    }
}

/// Wrapper so our `Value` can be bound as a tiberius parameter.
struct Param(Value);

impl ToSql for Param {
    fn to_sql(&self) -> ColumnData<'_> {
        match &self.0 {
            Value::Null => ColumnData::String(None),
            Value::Bool(b) => ColumnData::Bit(Some(*b)),
            Value::Int(i) => ColumnData::I64(Some(*i)),
            Value::Float(f) => ColumnData::F64(Some(*f)),
            Value::Text(s) => ColumnData::String(Some(s.as_str().into())),
            Value::Bytes(b) => ColumnData::Binary(Some(b.as_slice().into())),
            Value::Json(j) => ColumnData::String(Some(j.to_string().into())),
            Value::Timestamp(s) => ColumnData::String(Some(s.as_str().into())),
        }
    }
}

fn params_of(values: impl Iterator<Item = Value>) -> Vec<Param> {
    values.map(Param).collect()
}

fn param_refs(params: &[Param]) -> Vec<&dyn ToSql> {
    params.iter().map(|p| p as &dyn ToSql).collect()
}

fn ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn ms_value_from_row(row: &tiberius::Row, idx: usize) -> Value {
    use tiberius::ColumnType as CT;
    let Some(col) = row.columns().get(idx) else {
        return Value::Null;
    };

    macro_rules! try_as {
        ($t:ty, $ctor:expr) => {{
            if let Ok(Some(v)) = row.try_get::<$t, _>(idx) {
                return $ctor(v);
            }
        }};
    }

    let ty = col.column_type();
    match ty {
        CT::Null => return Value::Null,
        CT::Bit | CT::Bitn => try_as!(bool, Value::Bool),
        CT::Int1 => try_as!(u8, |v| Value::Int(v as i64)),
        CT::Int2 => try_as!(i16, |v| Value::Int(v as i64)),
        CT::Int4 => try_as!(i32, |v| Value::Int(v as i64)),
        CT::Int8 => try_as!(i64, Value::Int),
        CT::Intn => {
            try_as!(i64, Value::Int);
            try_as!(i32, |v| Value::Int(v as i64));
            try_as!(i16, |v| Value::Int(v as i64));
            try_as!(u8, |v| Value::Int(v as i64));
        }
        CT::Float4 => try_as!(f32, |v| Value::Float(v as f64)),
        CT::Float8 | CT::Money | CT::Money4 => try_as!(f64, Value::Float),
        CT::Floatn => {
            try_as!(f64, Value::Float);
            try_as!(f32, |v| Value::Float(v as f64));
        }
        CT::Decimaln | CT::Numericn => {
            try_as!(tiberius::numeric::Numeric, |v: tiberius::numeric::Numeric| {
                Value::Text(v.to_string())
            });
        }
        CT::Guid => try_as!(uuid::Uuid, |v: uuid::Uuid| Value::Text(v.to_string())),
        CT::BigVarChar | CT::BigChar | CT::NVarchar | CT::NChar | CT::Text | CT::NText
        | CT::Xml => {
            try_as!(&str, |v: &str| Value::Text(v.to_string()));
        }
        CT::BigVarBin | CT::BigBinary | CT::Image => {
            try_as!(&[u8], |v: &[u8]| Value::Bytes(v.to_vec()));
        }
        CT::Datetime | CT::Datetime4 | CT::Datetimen | CT::Datetime2 => {
            try_as!(chrono::NaiveDateTime, |v: chrono::NaiveDateTime| {
                Value::Timestamp(v.to_string())
            });
        }
        CT::DatetimeOffsetn => {
            try_as!(
                chrono::DateTime<chrono::Utc>,
                |v: chrono::DateTime<chrono::Utc>| Value::Timestamp(v.to_rfc3339())
            );
        }
        CT::Daten => {
            try_as!(chrono::NaiveDate, |v: chrono::NaiveDate| {
                Value::Timestamp(v.to_string())
            });
        }
        CT::Timen => {
            try_as!(chrono::NaiveTime, |v: chrono::NaiveTime| {
                Value::Timestamp(v.to_string())
            });
        }
        _ => {}
    }
    if let Ok(Some(v)) = row.try_get::<&str, _>(idx) {
        return Value::Text(v.to_string());
    }
    if row.try_get::<&str, _>(idx).map(|v| v.is_none()).unwrap_or(false) {
        return Value::Null;
    }
    Value::Text(format!("<{:?}>", ty))
}

/// Column metadata straight off the wire, so empty results still name their
/// columns (rows are the only other carrier of that metadata).
async fn stream_columns(
    stream: &mut tiberius::QueryStream<'_>,
) -> Result<Vec<MetaColumn>> {
    let cols = stream.columns().await?;
    Ok(cols
        .map(|cs| {
            cs.iter()
                .map(|c| MetaColumn {
                    name: c.name().to_string(),
                    type_name: format!("{:?}", c.column_type()),
                })
                .collect()
        })
        .unwrap_or_default())
}

fn result_set_from_rows(rows: Vec<tiberius::Row>) -> ResultSet {
    let columns: Vec<MetaColumn> = if let Some(first) = rows.first() {
        first
            .columns()
            .iter()
            .map(|c| MetaColumn {
                name: c.name().to_string(),
                type_name: format!("{:?}", c.column_type()),
            })
            .collect()
    } else {
        vec![]
    };
    let data: Vec<Vec<Value>> = rows
        .iter()
        .map(|r| (0..r.columns().len()).map(|i| ms_value_from_row(r, i)).collect())
        .collect();
    ResultSet { columns, rows: data, rows_affected: None }
}

fn update_sql(schema: &str, table: &str, changes: &RowChanges, pk: &PkValues) -> String {
    let mut param_idx = 1usize;
    let set_parts: Vec<String> = changes
        .keys()
        .map(|c| {
            let p = format!("{} = @P{}", ident(c), param_idx);
            param_idx += 1;
            p
        })
        .collect();
    let where_parts: Vec<String> = pk
        .keys()
        .map(|c| {
            let p = format!("{} = @P{}", ident(c), param_idx);
            param_idx += 1;
            p
        })
        .collect();
    format!(
        "UPDATE {}.{} SET {} WHERE {}",
        ident(schema),
        ident(table),
        set_parts.join(", "),
        where_parts.join(" AND ")
    )
}

fn delete_sql(schema: &str, table: &str, pk: &PkValues) -> String {
    let where_parts: Vec<String> = pk
        .keys()
        .enumerate()
        .map(|(i, c)| format!("{} = @P{}", ident(c), i + 1))
        .collect();
    format!(
        "DELETE FROM {}.{} WHERE {}",
        ident(schema),
        ident(table),
        where_parts.join(" AND ")
    )
}

async fn apply_changes_inner(
    client: &mut MsClient,
    schema: &str,
    table: &str,
    updates: &[(PkValues, RowChanges)],
    deletes: &[PkValues],
) -> Result<()> {
    for (pk, changes) in updates {
        if changes.is_empty() {
            continue;
        }
        if pk.is_empty() {
            return Err(anyhow!("update requires a primary key"));
        }
        let sql = update_sql(schema, table, changes, pk);
        let params = params_of(changes.values().cloned().chain(pk.values().cloned()));
        client.execute(sql, &param_refs(&params)).await?;
    }
    for pk in deletes {
        if pk.is_empty() {
            return Err(anyhow!("delete requires a primary key"));
        }
        let sql = delete_sql(schema, table, pk);
        let params = params_of(pk.values().cloned());
        client.execute(sql, &param_refs(&params)).await?;
    }
    Ok(())
}

/// Render a sys.types name + length/precision/scale into a printable type.
fn render_ms_type(ty: &str, max_length: i16, precision: u8, scale: u8) -> String {
    match ty {
        "varchar" | "char" | "varbinary" | "binary" => {
            if max_length == -1 {
                format!("{ty}(max)")
            } else {
                format!("{ty}({max_length})")
            }
        }
        "nvarchar" | "nchar" => {
            if max_length == -1 {
                format!("{ty}(max)")
            } else {
                format!("{ty}({})", max_length / 2)
            }
        }
        "decimal" | "numeric" => format!("{ty}({precision}, {scale})"),
        "datetime2" | "datetimeoffset" | "time" => format!("{ty}({scale})"),
        _ => ty.to_string(),
    }
}

fn opt_str(row: &tiberius::Row, idx: usize) -> String {
    row.try_get::<&str, _>(idx)
        .ok()
        .flatten()
        .unwrap_or_default()
        .to_string()
}

#[async_trait]
impl Driver for MsSqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::MsSql
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let mut client = self.client.lock().await;
        let rows = client
            .simple_query(
                "SELECT s.name FROM sys.schemas s \
                 WHERE s.name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') \
                   AND s.name NOT LIKE 'db[_]%' \
                 ORDER BY s.name",
            )
            .await?
            .into_first_result()
            .await?;
        Ok(rows.iter().map(|r| opt_str(r, 0)).collect())
    }

    async fn list_tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let mut client = self.client.lock().await;
        let rows = client
            .query(
                "SELECT table_name, table_type FROM information_schema.tables \
                 WHERE table_schema = @P1 ORDER BY table_name",
                &[&schema],
            )
            .await?
            .into_first_result()
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let kind = opt_str(r, 1);
                TableInfo {
                    schema: schema.to_string(),
                    name: opt_str(r, 0),
                    kind: if kind.contains("VIEW") { TableKind::View } else { TableKind::Table },
                }
            })
            .collect())
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableSchema> {
        let mut client = self.client.lock().await;
        let cols = client
            .query(
                "SELECT column_name, data_type, is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema = @P1 AND table_name = @P2 \
                 ORDER BY ordinal_position",
                &[&schema, &table],
            )
            .await?
            .into_first_result()
            .await?;

        let pk_rows = client
            .query(
                "SELECT kcu.column_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON tc.constraint_name = kcu.constraint_name \
                  AND tc.table_schema = kcu.table_schema \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                   AND tc.table_schema = @P1 AND tc.table_name = @P2 \
                 ORDER BY kcu.ordinal_position",
                &[&schema, &table],
            )
            .await?
            .into_first_result()
            .await?;
        let pk: Vec<String> = pk_rows.iter().map(|r| opt_str(r, 0)).collect();
        let pk_set: std::collections::HashSet<&str> = pk.iter().map(|s| s.as_str()).collect();

        let columns = cols
            .iter()
            .map(|r| {
                let name = opt_str(r, 0);
                ColumnSchema {
                    is_primary_key: pk_set.contains(name.as_str()),
                    nullable: opt_str(r, 2) == "YES",
                    type_name: opt_str(r, 1),
                    name,
                }
            })
            .collect();

        Ok(TableSchema { columns, primary_key: pk })
    }

    async fn query(&self, sql: &str) -> Result<ResultSet> {
        let trimmed = sql.trim_start().to_ascii_uppercase();
        let is_select = trimmed.starts_with("SELECT") || trimmed.starts_with("WITH");
        let mut client = self.client.lock().await;
        if is_select {
            let mut stream = client.simple_query(sql).await?;
            let meta = stream_columns(&mut stream).await?;
            let results = stream.into_results().await?;
            let rows = results.into_iter().find(|r| !r.is_empty()).unwrap_or_default();
            let mut rs = result_set_from_rows(rows);
            if rs.columns.is_empty() {
                rs.columns = meta;
            }
            Ok(rs)
        } else {
            let res = client.execute(sql, &[]).await?;
            Ok(ResultSet {
                columns: vec![],
                rows: vec![],
                rows_affected: Some(res.total()),
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
            "SELECT * FROM {}.{} ORDER BY (SELECT NULL) \
             OFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
            ident(schema),
            ident(table),
            offset,
            limit
        );
        let mut client = self.client.lock().await;
        let mut stream = client.simple_query(sql).await?;
        let meta = stream_columns(&mut stream).await?;
        let rows = stream.into_first_result().await?;
        let mut rs = result_set_from_rows(rows);
        if rs.columns.is_empty() {
            rs.columns = meta;
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
        let sql = update_sql(schema, table, changes, pk);
        let params = params_of(changes.values().cloned().chain(pk.values().cloned()));
        let mut client = self.client.lock().await;
        client.execute(sql, &param_refs(&params)).await?;
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
        let placeholders: Vec<String> = (1..=values.len()).map(|i| format!("@P{}", i)).collect();
        let sql = format!(
            "INSERT INTO {}.{} ({}) VALUES ({})",
            ident(schema),
            ident(table),
            cols.join(", "),
            placeholders.join(", ")
        );
        let params = params_of(values.values().cloned());
        let mut client = self.client.lock().await;
        client.execute(sql, &param_refs(&params)).await?;
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
        let sql = delete_sql(schema, table, pk);
        let params = params_of(pk.values().cloned());
        let mut client = self.client.lock().await;
        client.execute(sql, &param_refs(&params)).await?;
        Ok(())
    }

    async fn apply_changes(
        &self,
        schema: &str,
        table: &str,
        updates: &[(PkValues, RowChanges)],
        deletes: &[PkValues],
    ) -> Result<()> {
        let mut client = self.client.lock().await;
        client.execute("BEGIN TRANSACTION", &[]).await?;
        match apply_changes_inner(&mut client, schema, table, updates, deletes).await {
            Ok(()) => {
                client.execute("COMMIT TRANSACTION", &[]).await?;
                Ok(())
            }
            Err(e) => {
                let _ = client.execute("ROLLBACK TRANSACTION", &[]).await;
                Err(e)
            }
        }
    }

    async fn table_ddl(&self, schema: &str, table: &str, kind: TableKind) -> Result<String> {
        let qualified = format!("{}.{}", ident(schema), ident(table));
        let mut client = self.client.lock().await;

        if kind == TableKind::View {
            let rows = client
                .query(
                    "SELECT m.definition FROM sys.sql_modules m WHERE m.object_id = OBJECT_ID(@P1)",
                    &[&qualified.as_str()],
                )
                .await?
                .into_first_result()
                .await?;
            let def = rows
                .first()
                .map(|r| opt_str(r, 0))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("no definition found for view {qualified}"))?;
            return Ok(format!("{}\n", def.trim_end()));
        }

        // (name, type, max_length, precision, scale, is_nullable, is_identity, default definition)
        let col_rows = client
            .query(
                "SELECT c.name, ty.name, c.max_length, c.precision, c.scale, \
                        c.is_nullable, c.is_identity, dc.definition \
                 FROM sys.columns c \
                 JOIN sys.types ty ON c.user_type_id = ty.user_type_id \
                 LEFT JOIN sys.default_constraints dc \
                   ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
                 WHERE c.object_id = OBJECT_ID(@P1) \
                 ORDER BY c.column_id",
                &[&qualified.as_str()],
            )
            .await?
            .into_first_result()
            .await?;
        if col_rows.is_empty() {
            return Err(anyhow!("no columns found for {qualified}"));
        }

        let pk_rows = client
            .query(
                "SELECT tc.constraint_name, kcu.column_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON tc.constraint_name = kcu.constraint_name \
                  AND tc.table_schema = kcu.table_schema \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                   AND tc.table_schema = @P1 AND tc.table_name = @P2 \
                 ORDER BY kcu.ordinal_position",
                &[&schema, &table],
            )
            .await?
            .into_first_result()
            .await?;

        let fk_rows = client
            .query(
                "SELECT fk.name, c1.name, sch2.name, t2.name, c2.name \
                 FROM sys.foreign_key_columns fkc \
                 JOIN sys.foreign_keys fk ON fk.object_id = fkc.constraint_object_id \
                 JOIN sys.columns c1 ON fkc.parent_object_id = c1.object_id \
                  AND fkc.parent_column_id = c1.column_id \
                 JOIN sys.tables t2 ON fkc.referenced_object_id = t2.object_id \
                 JOIN sys.schemas sch2 ON t2.schema_id = sch2.schema_id \
                 JOIN sys.columns c2 ON fkc.referenced_object_id = c2.object_id \
                  AND fkc.referenced_column_id = c2.column_id \
                 WHERE fkc.parent_object_id = OBJECT_ID(@P1) \
                 ORDER BY fk.name, fkc.constraint_column_id",
                &[&qualified.as_str()],
            )
            .await?
            .into_first_result()
            .await?;

        let mut parts: Vec<String> = Vec::new();
        for r in &col_rows {
            let name = opt_str(r, 0);
            let ty = opt_str(r, 1);
            let max_length: i16 = r.try_get(2).ok().flatten().unwrap_or(0);
            let precision: u8 = r.try_get(3).ok().flatten().unwrap_or(0);
            let scale: u8 = r.try_get(4).ok().flatten().unwrap_or(0);
            let nullable: bool = r.try_get(5).ok().flatten().unwrap_or(true);
            let is_identity: bool = r.try_get(6).ok().flatten().unwrap_or(false);
            let default_def = {
                let d = opt_str(r, 7);
                if d.is_empty() { None } else { Some(d) }
            };

            let rendered_ty = match ty.as_str() {
                "varchar" | "char" | "varbinary" | "binary" => {
                    if max_length == -1 {
                        format!("{ty}(max)")
                    } else {
                        format!("{ty}({max_length})")
                    }
                }
                "nvarchar" | "nchar" => {
                    if max_length == -1 {
                        format!("{ty}(max)")
                    } else {
                        format!("{ty}({})", max_length / 2)
                    }
                }
                "decimal" | "numeric" => format!("{ty}({precision}, {scale})"),
                "datetime2" | "datetimeoffset" | "time" => format!("{ty}({scale})"),
                _ => ty.clone(),
            };

            let mut line = format!("    {} {}", ident(&name), rendered_ty);
            if is_identity {
                line.push_str(" IDENTITY(1,1)");
            }
            if let Some(d) = default_def {
                line.push_str(&format!(" DEFAULT {d}"));
            }
            if !nullable {
                line.push_str(" NOT NULL");
            }
            parts.push(line);
        }

        if !pk_rows.is_empty() {
            let pk_name = opt_str(&pk_rows[0], 0);
            let cols: Vec<String> = pk_rows.iter().map(|r| ident(&opt_str(r, 1))).collect();
            parts.push(format!(
                "    CONSTRAINT {} PRIMARY KEY ({})",
                ident(&pk_name),
                cols.join(", ")
            ));
        }

        // Group multi-column FKs by constraint name (rows are ordered by name, position).
        let mut fk_groups: Vec<(String, Vec<String>, String, Vec<String>)> = Vec::new();
        for r in &fk_rows {
            let name = opt_str(r, 0);
            let from_col = ident(&opt_str(r, 1));
            let to_ref = format!("{}.{}", ident(&opt_str(r, 2)), ident(&opt_str(r, 3)));
            let to_col = ident(&opt_str(r, 4));
            match fk_groups.last_mut() {
                Some((n, from, _, to)) if *n == name => {
                    from.push(from_col);
                    to.push(to_col);
                }
                _ => fk_groups.push((name, vec![from_col], to_ref, vec![to_col])),
            }
        }
        for (name, from, to_ref, to) in fk_groups {
            parts.push(format!(
                "    CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
                ident(&name),
                from.join(", "),
                to_ref,
                to.join(", ")
            ));
        }

        Ok(format!(
            "-- Note: non-PK indexes are not included for SQL Server.\n\
             CREATE TABLE {qualified} (\n{}\n);\n",
            parts.join(",\n")
        ))
    }

    async fn describe_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        let qualified = format!("{}.{}", ident(schema), ident(table));
        let mut client = self.client.lock().await;

        // (name, type, max_length, precision, scale, is_nullable, is_identity,
        //  is_computed, default definition, default constraint name)
        let col_rows = client
            .query(
                "SELECT c.name, ty.name, c.max_length, c.precision, c.scale, \
                        c.is_nullable, c.is_identity, c.is_computed, dc.definition, dc.name \
                 FROM sys.columns c \
                 JOIN sys.types ty ON c.user_type_id = ty.user_type_id \
                 LEFT JOIN sys.default_constraints dc \
                   ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
                 WHERE c.object_id = OBJECT_ID(@P1) \
                 ORDER BY c.column_id",
                &[&qualified.as_str()],
            )
            .await?
            .into_first_result()
            .await?;
        if col_rows.is_empty() {
            return Err(anyhow!("no columns found for {qualified}"));
        }

        let pk_rows = client
            .query(
                "SELECT tc.constraint_name, kcu.column_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON tc.constraint_name = kcu.constraint_name \
                  AND tc.table_schema = kcu.table_schema \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                   AND tc.table_schema = @P1 AND tc.table_name = @P2 \
                 ORDER BY kcu.ordinal_position",
                &[&schema, &table],
            )
            .await?
            .into_first_result()
            .await?;

        let fk_rows = client
            .query(
                "SELECT fk.name, c1.name, sch2.name, t2.name, c2.name, \
                        fk.delete_referential_action_desc, fk.update_referential_action_desc \
                 FROM sys.foreign_key_columns fkc \
                 JOIN sys.foreign_keys fk ON fk.object_id = fkc.constraint_object_id \
                 JOIN sys.columns c1 ON fkc.parent_object_id = c1.object_id \
                  AND fkc.parent_column_id = c1.column_id \
                 JOIN sys.tables t2 ON fkc.referenced_object_id = t2.object_id \
                 JOIN sys.schemas sch2 ON t2.schema_id = sch2.schema_id \
                 JOIN sys.columns c2 ON fkc.referenced_object_id = c2.object_id \
                  AND fkc.referenced_column_id = c2.column_id \
                 WHERE fkc.parent_object_id = OBJECT_ID(@P1) \
                 ORDER BY fk.name, fkc.constraint_column_id",
                &[&qualified.as_str()],
            )
            .await?
            .into_first_result()
            .await?;

        let idx_rows = client
            .query(
                "SELECT i.name, i.is_unique, c.name \
                 FROM sys.indexes i \
                 JOIN sys.index_columns ic \
                   ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                 JOIN sys.columns c \
                   ON c.object_id = i.object_id AND c.column_id = ic.column_id \
                 WHERE i.object_id = OBJECT_ID(@P1) \
                   AND i.is_primary_key = 0 AND i.is_unique_constraint = 0 \
                   AND i.type > 0 AND i.name IS NOT NULL AND ic.key_ordinal > 0 \
                 ORDER BY i.name, ic.key_ordinal",
                &[&qualified.as_str()],
            )
            .await?
            .into_first_result()
            .await?;

        let check_rows = client
            .query(
                "SELECT name, definition FROM sys.check_constraints \
                 WHERE parent_object_id = OBJECT_ID(@P1) ORDER BY name",
                &[&qualified.as_str()],
            )
            .await?
            .into_first_result()
            .await?;

        let columns = col_rows
            .iter()
            .map(|r| {
                let name = opt_str(r, 0);
                let ty = opt_str(r, 1);
                let max_length: i16 = r.try_get(2).ok().flatten().unwrap_or(0);
                let precision: u8 = r.try_get(3).ok().flatten().unwrap_or(0);
                let scale: u8 = r.try_get(4).ok().flatten().unwrap_or(0);
                let nullable: bool = r.try_get(5).ok().flatten().unwrap_or(true);
                let is_identity: bool = r.try_get(6).ok().flatten().unwrap_or(false);
                let is_computed: bool = r.try_get(7).ok().flatten().unwrap_or(false);
                let default = {
                    let d = opt_str(r, 8);
                    if d.is_empty() { None } else { Some(d) }
                };
                let default_constraint = {
                    let d = opt_str(r, 9);
                    if d.is_empty() { None } else { Some(d) }
                };
                ColumnInfo {
                    name,
                    type_name: render_ms_type(&ty, max_length, precision, scale),
                    not_null: !nullable,
                    default,
                    identity: if is_identity { IdentityKind::MsIdentity } else { IdentityKind::None },
                    generated: is_computed,
                    default_constraint,
                }
            })
            .collect();

        let primary_key = if pk_rows.is_empty() {
            None
        } else {
            Some(KeyInfo {
                name: Some(opt_str(&pk_rows[0], 0)),
                columns: pk_rows.iter().map(|r| opt_str(r, 1)).collect(),
            })
        };

        fn action(desc: &str) -> String {
            desc.replace('_', " ")
        }

        let mut foreign_keys: Vec<FkInfo> = Vec::new();
        for r in &fk_rows {
            let name = opt_str(r, 0);
            let col = opt_str(r, 1);
            let ref_col = opt_str(r, 4);
            match foreign_keys.last_mut() {
                Some(last) if last.name == name => {
                    last.columns.push(col);
                    last.ref_columns.push(ref_col);
                }
                _ => foreign_keys.push(FkInfo {
                    name,
                    columns: vec![col],
                    ref_schema: opt_str(r, 2),
                    ref_table: opt_str(r, 3),
                    ref_columns: vec![ref_col],
                    on_delete: action(&opt_str(r, 5)),
                    on_update: action(&opt_str(r, 6)),
                }),
            }
        }

        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in &idx_rows {
            let name = opt_str(r, 0);
            let unique: bool = r.try_get(1).ok().flatten().unwrap_or(false);
            let col = opt_str(r, 2);
            match indexes.last_mut() {
                Some(last) if last.name == name => last.columns.push(col),
                _ => indexes.push(IndexInfo { name, columns: vec![col], unique }),
            }
        }

        let checks = check_rows
            .iter()
            .map(|r| {
                let def = opt_str(r, 1);
                let t = def.trim();
                let expr = if t.starts_with('(') && t.ends_with(')') {
                    t[1..t.len() - 1].to_string()
                } else {
                    t.to_string()
                };
                CheckInfo { name: opt_str(r, 0), expression: expr }
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
        let mut client = self.client.lock().await;
        if let Err(e) = client.execute("BEGIN TRANSACTION", &[]).await {
            return DdlOutcome { applied: 0, error: Some(format!("{e:#}")) };
        }
        for (i, stmt) in statements.iter().enumerate() {
            if let Err(e) = client.execute(stmt.as_str(), &[]).await {
                let _ = client.execute("ROLLBACK TRANSACTION", &[]).await;
                return DdlOutcome {
                    applied: 0,
                    error: Some(format!("statement {} failed (rolled back): {e:#}", i + 1)),
                };
            }
        }
        match client.execute("COMMIT TRANSACTION", &[]).await {
            Ok(_) => DdlOutcome { applied: statements.len(), error: None },
            Err(e) => {
                let _ = client.execute("ROLLBACK TRANSACTION", &[]).await;
                DdlOutcome { applied: 0, error: Some(format!("commit failed: {e:#}")) }
            }
        }
    }

    async fn fetch_db_meta(&self) -> Result<DbMeta> {
        let mut client = self.client.lock().await;

        let default_schema = client
            .simple_query("SELECT COALESCE(SCHEMA_NAME(), 'dbo')")
            .await?
            .into_first_result()
            .await?
            .first()
            .map(|r| opt_str(r, 0))
            .unwrap_or_else(|| "dbo".to_string());

        let table_rows = client
            .simple_query(
                "SELECT table_schema, table_name, table_type \
                 FROM information_schema.tables \
                 WHERE table_schema NOT IN ('sys', 'INFORMATION_SCHEMA') \
                 ORDER BY table_schema, table_name",
            )
            .await?
            .into_first_result()
            .await?;

        let col_rows = client
            .simple_query(
                "SELECT table_schema, table_name, column_name, data_type, is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema NOT IN ('sys', 'INFORMATION_SCHEMA') \
                 ORDER BY table_schema, table_name, ordinal_position",
            )
            .await?
            .into_first_result()
            .await?;

        let pk_rows = client
            .simple_query(
                "SELECT tc.table_schema, tc.table_name, kcu.column_name \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON tc.constraint_name = kcu.constraint_name \
                  AND tc.table_schema    = kcu.table_schema \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                 ORDER BY tc.table_schema, tc.table_name, kcu.ordinal_position",
            )
            .await?
            .into_first_result()
            .await?;

        let fk_rows = client
            .simple_query(
                "SELECT sch1.name, t1.name, c1.name, sch2.name, t2.name, c2.name \
                 FROM sys.foreign_key_columns fkc \
                 JOIN sys.tables t1 ON fkc.parent_object_id = t1.object_id \
                 JOIN sys.schemas sch1 ON t1.schema_id = sch1.schema_id \
                 JOIN sys.columns c1 ON fkc.parent_object_id = c1.object_id \
                  AND fkc.parent_column_id = c1.column_id \
                 JOIN sys.tables t2 ON fkc.referenced_object_id = t2.object_id \
                 JOIN sys.schemas sch2 ON t2.schema_id = sch2.schema_id \
                 JOIN sys.columns c2 ON fkc.referenced_object_id = c2.object_id \
                  AND fkc.referenced_column_id = c2.column_id",
            )
            .await?
            .into_first_result()
            .await?;

        type Key = (String, String);
        let mut by_key: HashMap<Key, TableMeta> = HashMap::new();
        for r in &table_rows {
            let schema = opt_str(r, 0);
            let name = opt_str(r, 1);
            let kind = opt_str(r, 2);
            by_key.insert(
                (schema.clone(), name.clone()),
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
        for r in &col_rows {
            let key = (opt_str(r, 0), opt_str(r, 1));
            if let Some(t) = by_key.get_mut(&key) {
                t.columns.push(ColumnSchema {
                    name: opt_str(r, 2),
                    type_name: opt_str(r, 3),
                    nullable: opt_str(r, 4) == "YES",
                    is_primary_key: false,
                });
            }
        }
        for r in &pk_rows {
            let key = (opt_str(r, 0), opt_str(r, 1));
            let col = opt_str(r, 2);
            if let Some(t) = by_key.get_mut(&key) {
                t.primary_key.push(col.clone());
                if let Some(c) = t.columns.iter_mut().find(|c| c.name == col) {
                    c.is_primary_key = true;
                }
            }
        }
        for r in &fk_rows {
            let key = (opt_str(r, 0), opt_str(r, 1));
            if let Some(t) = by_key.get_mut(&key) {
                t.foreign_keys.push(ForeignKey {
                    from_column: opt_str(r, 2),
                    to_schema: opt_str(r, 3),
                    to_table: opt_str(r, 4),
                    to_column: opt_str(r, 5),
                });
            }
        }

        let mut tables: Vec<TableMeta> = by_key.into_values().collect();
        tables.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
        Ok(DbMeta { tables, default_schema })
    }
}
