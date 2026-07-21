//! End-to-end test of the Postgres driver against a local test database.
//!
//! Requires env `DBTOOL_TEST_PG_URL`, e.g.
//!   postgres://dbtool_test:dbtool_test_pw@127.0.0.1:5432/dbtool_test
//! Test is skipped (prints "skipped") if the var isn't set.

use dbtool::db::{self, ConnectParams, DbKind, Value};

fn pg_url() -> Option<String> {
    std::env::var("DBTOOL_TEST_PG_URL").ok()
}

fn parse_url(url: &str) -> ConnectParams {
    // Minimal parser — only supports what our test URL looks like.
    let without_scheme = url.strip_prefix("postgres://").expect("postgres://");
    let (creds, rest) = without_scheme.split_once('@').unwrap();
    let (user, password) = creds.split_once(':').unwrap();
    let (hostport, db) = rest.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    ConnectParams {
        kind: DbKind::Postgres,
        host: host.to_string(),
        port: port.parse().unwrap(),
        database: db.to_string(),
        username: user.to_string(),
        password: password.to_string(),
        require_ssl: false,
        ssh: None,
    }
}

#[tokio::test]
async fn driver_end_to_end() {
    let Some(url) = pg_url() else {
        eprintln!("DBTOOL_TEST_PG_URL not set — skipping");
        return;
    };
    let params = parse_url(&url);
    let (driver, _tunnel) = db::connect(&params).await.expect("connect");

    let schemas = driver.list_schemas().await.expect("list_schemas");
    assert!(schemas.iter().any(|s| s == "public"), "public schema present: {schemas:?}");

    let tables = driver.list_tables("public").await.expect("list_tables");
    let names: Vec<_> = tables.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"people"));
    assert!(names.contains(&"no_pk_table"));

    let ts = driver.describe_table("public", "people").await.expect("describe");
    assert_eq!(ts.primary_key, vec!["id".to_string()]);
    assert!(ts.columns.iter().any(|c| c.name == "name"));

    // Fetch rows.
    let rs = driver.fetch_table_rows("public", "people", 100, 0, &Default::default()).await.expect("fetch rows");
    assert_eq!(rs.rows.len(), 3);

    // SELECT via query().
    let q = driver.query("SELECT COUNT(*) FROM people").await.expect("query");
    assert_eq!(q.rows.len(), 1);

    // Insert.
    let mut insert = db::RowChanges::new();
    insert.insert("name".into(), Value::Text("Dana".into()));
    insert.insert("age".into(), Value::Int(22));
    insert.insert("email".into(), Value::Text("dana@example.com".into()));
    driver.insert_row("public", "people", &insert).await.expect("insert");

    // Find Dana's id.
    let rs = driver.query("SELECT id FROM people WHERE name = 'Dana'").await.expect("sel dana");
    let dana_id = match &rs.rows[0][0] {
        Value::Int(i) => *i,
        other => panic!("expected int, got {other:?}"),
    };

    // Update.
    let mut pk = db::PkValues::new();
    pk.insert("id".into(), Value::Int(dana_id));
    let mut changes = db::RowChanges::new();
    changes.insert("age".into(), Value::Int(23));
    driver.update_row("public", "people", &pk, &changes).await.expect("update");

    let rs = driver.query(&format!("SELECT age FROM people WHERE id = {dana_id}")).await.expect("check age");
    assert!(matches!(&rs.rows[0][0], Value::Int(23)));

    // Delete.
    driver.delete_row("public", "people", &pk).await.expect("delete");
    let rs = driver.query(&format!("SELECT COUNT(*) FROM people WHERE id = {dana_id}")).await.expect("verify del");
    let count = match &rs.rows[0][0] {
        Value::Int(n) => *n,
        Value::Text(t) => t.parse::<i64>().unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(count, 0);

    // no_pk_table: describe_table should return empty PK.
    let ts = driver.describe_table("public", "no_pk_table").await.expect("describe no_pk");
    assert!(!ts.has_primary_key());

    // Empty tables must still report their columns (regression: a freshly
    // created table rendered as "(no columns)" in the data grid).
    driver
        .query("CREATE TABLE \"emptyCamel\" (id bigint, \"createdOn\" timestamptz)")
        .await
        .expect("create empty table");
    let rs = driver.fetch_table_rows("public", "emptyCamel", 10, 0, &Default::default()).await.expect("fetch empty");
    assert_eq!(rs.rows.len(), 0);
    let names: Vec<_> = rs.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "createdOn"]);
    let q = driver.query("SELECT * FROM \"emptyCamel\"").await.expect("empty select");
    assert_eq!(q.columns.len(), 2);
    driver.query("DROP TABLE \"emptyCamel\"").await.expect("drop empty table");
}

#[tokio::test]
async fn ddl_dump_and_csv_import() {
    let Some(url) = pg_url() else {
        eprintln!("DBTOOL_TEST_PG_URL not set — skipping");
        return;
    };
    let params = parse_url(&url);
    let (driver, _tunnel) = db::connect(&params).await.expect("connect");

    // DDL generation for a table with PK + FK.
    let ddl = driver
        .table_ddl("public", "people", db::TableKind::Table)
        .await
        .expect("people ddl");
    assert!(ddl.contains("CREATE TABLE \"public\".\"people\""), "ddl:\n{ddl}");
    assert!(ddl.contains("PRIMARY KEY"), "ddl:\n{ddl}");

    let ddl = driver
        .table_ddl("public", "addresses", db::TableKind::Table)
        .await
        .expect("addresses ddl");
    assert!(ddl.contains("CREATE TABLE \"public\".\"addresses\""), "ddl:\n{ddl}");

    // Full dump: one .sql per object under <tmp>/public/.
    let tmp = std::env::temp_dir().join(format!("dbtool-ddl-test-{}", std::process::id()));
    let (files, errors) = dbtool::runtime::dump_ddl(
        driver.clone(),
        vec!["public".to_string()],
        tmp.to_str().unwrap(),
    )
    .await
    .expect("dump");
    assert!(errors.is_empty(), "dump errors: {errors:?}");
    assert!(files >= 3, "expected >= 3 files, got {files}");
    assert!(tmp.join("public").join("people.sql").exists());
    let _ = std::fs::remove_dir_all(&tmp);

    // CSV import into a scratch table.
    driver
        .query("DROP TABLE IF EXISTS csv_import_target")
        .await
        .expect("drop scratch");
    driver
        .query("CREATE TABLE csv_import_target (id int, name text, note text)")
        .await
        .expect("create scratch");
    let csv_path = std::env::temp_dir().join(format!("dbtool-csv-test-{}.csv", std::process::id()));
    std::fs::write(&csv_path, "name,id,note\nAlice,1,hello\n\"O'Brien\",2,\nBob,3,\"line\nbreak\"\n")
        .expect("write csv");

    let rows = dbtool::runtime::import_csv(
        driver.clone(),
        "public",
        "csv_import_target",
        csv_path.to_str().unwrap(),
        &dbtool::csv_import::ImportOptions::default(),
        |_| {},
    )
    .await
    .expect("import");
    assert_eq!(rows, 3);

    let rs = driver
        .query("SELECT name, id, note FROM csv_import_target ORDER BY id")
        .await
        .expect("verify");
    assert_eq!(rs.rows.len(), 3);
    assert!(matches!(&rs.rows[1][0], Value::Text(t) if t == "O'Brien"));
    // Empty field imported as NULL.
    assert!(matches!(&rs.rows[1][2], Value::Null));
    // Quoted embedded newline survives.
    assert!(matches!(&rs.rows[2][2], Value::Text(t) if t == "line\nbreak"));

    driver.query("DROP TABLE csv_import_target").await.expect("cleanup");
    let _ = std::fs::remove_file(&csv_path);
}
