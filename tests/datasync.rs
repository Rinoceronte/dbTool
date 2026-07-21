//! End-to-end data compare / sync / sanitized pull against two temp SQLite
//! databases — exercises the real drivers, no external server needed.

use dbtool::db::datasync::{self, MaskMap, MaskStrategy, TableSel};
use dbtool::db::{self, ConnectParams, DbKind, Value};

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

fn people_sel() -> TableSel {
    TableSel {
        schema: "main".into(),
        table: "people".into(),
        columns: vec!["id".into(), "email".into(), "name".into(), "age".into()],
        pk: vec!["id".into()],
    }
}

#[tokio::test]
async fn compare_sync_and_pull() {
    let dir = std::env::temp_dir().join(format!("dbtool-datasync-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src_file = dir.join("source.db");
    let tgt_file = dir.join("target.db");
    let _ = std::fs::remove_file(&src_file);
    let _ = std::fs::remove_file(&tgt_file);

    let (source, _t1) = db::connect(&params(&src_file)).await.expect("source");
    let (target, _t2) = db::connect(&params(&tgt_file)).await.expect("target");

    let ddl = "CREATE TABLE people (id INTEGER PRIMARY KEY, email TEXT, name TEXT, age INTEGER)";
    source.query(ddl).await.unwrap();
    target.query(ddl).await.unwrap();

    // Source: 1..=4. Target: shares 1 & 2 (2 drifted), misses 3 & 4, has 9.
    source
        .query(
            "INSERT INTO people VALUES \
             (1, 'ada@corp.com', 'Ada', 36), \
             (2, 'grace@corp.com', 'Grace', 45), \
             (3, 'linus@corp.com', 'Linus', 28), \
             (4, 'edsger@corp.com', 'Edsger', 55)",
        )
        .await
        .unwrap();
    target
        .query(
            "INSERT INTO people VALUES \
             (1, 'ada@corp.com', 'Ada', 36), \
             (2, 'grace@corp.com', 'Grace', 44), \
             (9, 'stray@corp.com', 'Stray', 1)",
        )
        .await
        .unwrap();

    // --- Compare -----------------------------------------------------------
    let sel = people_sel();
    let report = datasync::compare_table(&source, &target, &sel, 1000).await;
    assert!(report.error.is_none(), "{:?}", report.error);
    assert_eq!(report.source_count, Some(4));
    assert_eq!(report.target_count, Some(3));
    assert_eq!(report.missing.len(), 2, "rows 3 & 4 missing on target");
    assert_eq!(report.extra.len(), 1, "row 9 only on target");
    assert_eq!(report.changed.len(), 1, "row 2 drifted");
    assert!(!report.in_sync());

    // --- Sync script makes target match source -----------------------------
    let script = datasync::sync_script(DbKind::Sqlite, &report, &MaskMap::new(), true);
    assert!(script.contains("INSERT INTO"));
    assert!(script.contains("UPDATE"));
    assert!(script.contains("DELETE"));
    target.query(&script).await.expect("apply sync script");

    let after = datasync::compare_table(&source, &target, &sel, 1000).await;
    assert!(after.in_sync(), "after sync: {after:?}");

    // --- Sanitized pull ----------------------------------------------------
    let mut masks = MaskMap::new();
    masks.insert("main.people.email".into(), MaskStrategy::HashEmail);
    masks.insert("main.people.name".into(), MaskStrategy::Hash);
    let progress = |_msg: String| {};
    let written = datasync::pull_table(&source, &target, &sel, &masks, None, &progress)
        .await
        .expect("pull");
    assert_eq!(written, 4);

    let rows = target
        .query("SELECT email, name FROM people ORDER BY id")
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 4);
    for row in &rows.rows {
        let email = row[0].display();
        let name = row[1].display();
        assert!(
            email.ends_with("@example.test"),
            "email not masked: {email}"
        );
        assert!(!email.contains("corp.com"));
        assert_eq!(name.len(), 16, "name should be a 64-bit hex hash: {name}");
    }
    // Determinism: same source value → same masked value on a second pull.
    let first: Vec<String> = rows.rows.iter().map(|r| r[0].display()).collect();
    datasync::pull_table(&source, &target, &sel, &masks, None, &progress)
        .await
        .unwrap();
    let again = target
        .query("SELECT email FROM people ORDER BY id")
        .await
        .unwrap();
    let second: Vec<String> = again.rows.iter().map(|r| r[0].display()).collect();
    assert_eq!(first, second);

    // Row limit caps the copy.
    let limited = datasync::pull_table(&source, &target, &sel, &masks, Some(2), &progress)
        .await
        .unwrap();
    assert_eq!(limited, 2);
    let n = target.query("SELECT count(*) FROM people").await.unwrap();
    assert!(matches!(n.rows[0][0], Value::Int(2)));

    let _ = std::fs::remove_dir_all(&dir);
}
