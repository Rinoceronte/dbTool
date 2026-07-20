pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod structure;
pub mod types;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub use types::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbKind {
    Postgres,
    MySql,
    MsSql,
}

impl DbKind {
    pub fn display(&self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::MySql => "MySQL / MariaDB",
            DbKind::MsSql => "Microsoft SQL Server",
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            DbKind::Postgres => 5432,
            DbKind::MySql => 3306,
            DbKind::MsSql => 1433,
        }
    }
}

#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DbKind;
    async fn list_schemas(&self) -> Result<Vec<String>>;
    async fn list_tables(&self, schema: &str) -> Result<Vec<TableInfo>>;
    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableSchema>;
    async fn query(&self, sql: &str) -> Result<ResultSet>;
    async fn fetch_table_rows(
        &self,
        schema: &str,
        table: &str,
        limit: i64,
        offset: i64,
    ) -> Result<ResultSet>;
    async fn update_row(
        &self,
        schema: &str,
        table: &str,
        pk: &PkValues,
        changes: &RowChanges,
    ) -> Result<()>;
    async fn insert_row(
        &self,
        schema: &str,
        table: &str,
        values: &RowChanges,
    ) -> Result<()>;
    async fn delete_row(
        &self,
        schema: &str,
        table: &str,
        pk: &PkValues,
    ) -> Result<()>;
    async fn apply_changes(
        &self,
        schema: &str,
        table: &str,
        updates: &[(PkValues, RowChanges)],
        deletes: &[PkValues],
    ) -> Result<()>;
    async fn fetch_db_meta(&self) -> Result<DbMeta>;
    /// Generate a `CREATE TABLE` / `CREATE VIEW` statement for one object,
    /// suitable for writing to a DDL file.
    async fn table_ddl(&self, schema: &str, table: &str, kind: TableKind) -> Result<String>;
    /// Full structural introspection (columns with defaults/identity, PK,
    /// FKs, secondary indexes, checks) for the Structure editor.
    async fn describe_structure(&self, schema: &str, table: &str) -> Result<structure::TableStructure>;
    /// Execute a DDL batch: transactionally where the dialect supports it
    /// (Postgres, SQL Server), sequentially with stop-on-error otherwise
    /// (MySQL). Returns how many statements actually took effect.
    async fn apply_ddl(&self, statements: &[String]) -> structure::DdlOutcome;
}

pub type DynDriver = Arc<dyn Driver>;

#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    pub require_ssl: bool,
}

impl ConnectParams {
    pub fn to_url(&self) -> String {
        let scheme = match self.kind {
            DbKind::Postgres => "postgres",
            DbKind::MySql => "mysql",
            DbKind::MsSql => "mssql",
        };
        let user = urlencoding::encode(&self.username);
        let pw = urlencoding::encode(&self.password);
        let db = urlencoding::encode(&self.database);
        let mut url = format!(
            "{scheme}://{user}:{pw}@{host}:{port}/{db}",
            host = self.host,
            port = self.port,
        );
        if self.require_ssl {
            let sep = if url.contains('?') { '&' } else { '?' };
            match self.kind {
                DbKind::Postgres => url.push_str(&format!("{sep}sslmode=require")),
                DbKind::MySql => url.push_str(&format!("{sep}ssl-mode=REQUIRED")),
                DbKind::MsSql => url.push_str(&format!("{sep}encrypt=true")),
            }
        }
        url
    }
}

pub async fn connect(params: &ConnectParams) -> Result<DynDriver> {
    match params.kind {
        DbKind::Postgres => Ok(Arc::new(postgres::PostgresDriver::connect(params).await?)),
        DbKind::MySql => Ok(Arc::new(mysql::MySqlDriver::connect(params).await?)),
        DbKind::MsSql => Ok(Arc::new(mssql::MsSqlDriver::connect(params).await?)),
    }
}
