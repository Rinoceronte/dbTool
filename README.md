# dbTool

A fast, native desktop database client written in Rust ([egui](https://github.com/emilk/egui) + [sqlx](https://github.com/launchbadge/sqlx)).

Supports **PostgreSQL**, **MySQL**, **Microsoft SQL Server**, and **SQLite**.

## Features

- **SQL editor** — completion (schema-aware, fuzzy), multi-statement scripts, query parameters (`:name`), formatting, find/replace, error→cursor jump, saved snippets, persistent query history, per-tab auto-refresh
- **Results grid** — sorting/filtering, in-place cell editing, row insert/delete, FK hover-peek, column stats, copy/export as CSV / JSON / INSERT / Markdown, streamed row cap with "fetch all"
- **Schema tools** — tree browser (tables, views, functions, sequences, enums, triggers), table structure editor with generated ALTERs (including SQLite table-rebuilds), DDL dump, table/column comments, routine source viewing & editing
- **Diagrams** — dbdiagram-style DBML editor with pan/zoom canvas, draggable tables, FK curves; introspect any database straight into a diagram
- **Structure compare** — DataGrip-style migration diff between any two connections with per-change checkboxes
- **Data compare & sync** — checksum + PK-ordered row diff between databases, sync-script generation, and sanitized data pulls (in-flight column masking: NULL / fixed / deterministic hash) for prod→sandbox refreshes
- **Ops** — sessions monitor with kill, server-side query cancel, users & roles, visual EXPLAIN plans, whole-database dump/restore (pg_dump / mysqldump)
- **Safety** — read-only profiles (enforced write classifier) and production profiles (red-tinted UI, protected from data pulls)
- **Connections** — SSH tunnels, OS-keyring credential storage, auto-reconnect, session restore
- **AI** — embedded Claude agent panel (via the `claude` CLI + in-process MCP server) that can inspect schemas and run queries

## Installing

Grab a prebuilt binary for Linux, Windows, or macOS (Intel & Apple Silicon) from the
[Releases page](https://github.com/Rinoceronte/dbTool/releases) — unpack and run, no
runtime dependencies beyond a desktop environment.

Or build from source with a Rust toolchain:

```sh
cargo install --git https://github.com/Rinoceronte/dbTool
```

## Building

```sh
cargo build --release
```

The binary is `target/release/dbtool`. Requires a Rust toolchain (2024 edition).

Optional external tools: `ssh` (tunnels), `pg_dump`/`pg_restore`, `mysqldump`/`mysql` (dump & restore), `claude` (AI panel).

## Tests

```sh
cargo test
```

SQLite and datasync suites run anywhere; Postgres/MySQL/MSSQL integration tests expect local test servers (see the header comments in `tests/*.rs` for docker one-liners).

## License

[MIT](LICENSE)
