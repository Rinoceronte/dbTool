//! One-off: dump the test database's DDL to a folder. Usage:
//!   DBTOOL_TEST_PG_URL=... cargo run --example dump_ddl -- <out_dir>
use dbtool::db::{self, ConnectParams, DbKind};

#[tokio::main]
async fn main() {
    let url = std::env::var("DBTOOL_TEST_PG_URL").expect("DBTOOL_TEST_PG_URL");
    let out = std::env::args().nth(1).expect("out dir arg");
    let without_scheme = url.strip_prefix("postgres://").unwrap();
    let (creds, rest) = without_scheme.split_once('@').unwrap();
    let (user, password) = creds.split_once(':').unwrap();
    let (hostport, dbname) = rest.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let params = ConnectParams {
        kind: DbKind::Postgres,
        host: host.into(),
        port: port.parse().unwrap(),
        database: dbname.into(),
        username: user.into(),
        password: password.into(),
        require_ssl: false,
    };
    let driver = db::connect(&params).await.expect("connect");
    let (files, errors) = dbtool::runtime::dump_ddl(driver, vec![], &out).await.expect("dump");
    println!("wrote {files} file(s), {} error(s)", errors.len());
    for e in errors {
        eprintln!("  {e}");
    }
}
