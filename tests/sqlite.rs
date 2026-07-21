//! End-to-end test of the SQLite driver against a temp-file database.
//! Needs nothing external, so it always runs.

use dbtool::db::{self, ConnectParams, DbKind, RowsFilter, Value};

fn params(path: &std::path::Path) -> ConnectParams {
    ConnectParams {
        kind: DbKind::Sqlite,
        host: String::new(),
        port: 0,
        database: path.to_string_lossy().into_owned(),
        username: String::new(),
        password: String::new(),
        require_ssl: false,
        ssh: None,
    }
}

#[tokio::test]
async fn sqlite_end_to_end() {
    let dir = std::env::temp_dir().join(format!("dbtool-sqlite-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("test.db");
    let _ = std::fs::remove_file(&file);

    let (driver, _tunnel) = db::connect(&params(&file)).await.expect("connect");

    // DDL script (multi-statement) through the editor path.
    let sets = driver
        .query_script(
            "CREATE TABLE person (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, age INTEGER); \
             CREATE TABLE pet (id INTEGER PRIMARY KEY, owner_id INTEGER NOT NULL REFERENCES person(id), name TEXT); \
             CREATE INDEX idx_pet_owner ON pet(owner_id); \
             INSERT INTO person (name, age) VALUES ('Ada', 36), ('Grace', 45); \
             INSERT INTO pet (id, owner_id, name) VALUES (1, 1, 'Rex'); \
             SELECT COUNT(*) AS n FROM person",
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("script");
    let last = sets.last().unwrap();
    assert_eq!(last.rows.len(), 1);
    assert!(matches!(last.rows[0][0], Value::Int(2)));

    // Introspection.
    let schemas = driver.list_schemas().await.unwrap();
    assert_eq!(schemas, vec!["main"]);
    let tables = driver.list_tables("main").await.unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["person", "pet"]);

    let ts = driver.describe_table("main", "person").await.unwrap();
    assert_eq!(ts.primary_key, vec!["id"]);
    assert!(ts.columns.iter().any(|c| c.name == "name" && !c.nullable));

    let st = driver.describe_structure("main", "pet").await.unwrap();
    assert_eq!(st.foreign_keys.len(), 1);
    assert_eq!(st.foreign_keys[0].ref_table, "person");
    assert_eq!(st.indexes.len(), 1);
    assert_eq!(st.indexes[0].columns, vec!["owner_id"]);

    let ddl = driver.table_ddl("main", "pet", dbtool::db::TableKind::Table).await.unwrap();
    assert!(ddl.contains("CREATE TABLE"));
    assert!(ddl.contains("idx_pet_owner"));

    // Row CRUD via the grid paths.
    let mut values = dbtool::db::RowChanges::new();
    values.insert("name".into(), Value::Text("Linus".into()));
    values.insert("age".into(), Value::Int(28));
    driver.insert_row("main", "person", &values).await.unwrap();

    let rows = driver
        .fetch_table_rows("main", "person", 10, 0, &RowsFilter::default())
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 3);

    let mut pk = dbtool::db::PkValues::new();
    pk.insert("id".into(), Value::Int(3));
    let mut changes = dbtool::db::RowChanges::new();
    changes.insert("age".into(), Value::Int(29));
    driver.update_row("main", "person", &pk, &changes).await.unwrap();
    driver.delete_row("main", "person", &pk).await.unwrap();
    let rows = driver
        .fetch_table_rows("main", "person", 10, 0, &RowsFilter::default())
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 2);

    // FK enforcement is on.
    let orphan = driver.query("INSERT INTO pet (id, owner_id) VALUES (9, 999)").await;
    assert!(orphan.is_err(), "FK violation must fail");

    // Meta for the completion cache / diagrams.
    let meta = driver.fetch_db_meta().await.unwrap();
    assert_eq!(meta.default_schema, "main");
    assert_eq!(meta.tables.len(), 2);
    let pet = meta.tables.iter().find(|t| t.name == "pet").unwrap();
    assert_eq!(pet.foreign_keys.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}
