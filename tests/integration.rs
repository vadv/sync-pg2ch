use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clickhouse::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tempfile::TempDir;
use url::Url;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[path = "integration/ddl.rs"]
mod ddl;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    pg_columns: String,
    ch_columns: String,
    status: String,
    #[serde(default)]
    postgis: bool,
    #[serde(default)]
    pg_suffix: String,
    #[serde(default)]
    pg_after: Vec<String>,
    #[serde(default)]
    ignore: Vec<String>,
    code: Option<String>,
    geo: Option<Value>,
}

struct Environment {
    admin: PgPool,
    pg: PgPool,
    ch: Client,
    pg_url: String,
    ch_url: String,
    ch_user: String,
    ch_password: String,
    database: String,
    schema: String,
    cases: Vec<Case>,
    files: TempDir,
}

fn required_env(name: &str) -> TestResult<String> {
    let value = std::env::var(name)
        .map_err(|_| format!("{name} is required; provision the integration services first"))?;
    if value.is_empty() {
        return Err(format!("{name} must not be empty").into());
    }
    Ok(value)
}

fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn exit_code(status: &str) -> i32 {
    match status {
        "match" | "compatible" => 0,
        "mismatch" => 1,
        "unknown" => 2,
        value => panic!("invalid fixture status: {value}"),
    }
}

impl Environment {
    async fn create() -> TestResult<Self> {
        let pg_url = required_env("PG_URL")?;
        let ch_url = required_env("CH_URL")?;
        let ch_user = required_env("CH_USER")?;
        let ch_password = required_env("CH_PASSWORD")?;
        let postgis = match required_env("TEST_POSTGIS")?.as_str() {
            "true" => true,
            "false" => false,
            value => return Err(format!("TEST_POSTGIS must be true or false, got {value}").into()),
        };
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let database = format!("sync_pg2ch_test_{}_{suffix}", std::process::id());
        let schema = "source.with.dot".to_owned();
        let admin = PgPool::connect(&pg_url).await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE {} TEMPLATE template0",
            identifier(&database)
        )))
        .execute(&admin)
        .await?;
        let mut pg_url = Url::parse(&pg_url)?;
        pg_url.set_path(&database);
        let pg_url = pg_url.to_string();
        let pg = PgPool::connect(&pg_url).await?;
        let ch = Client::default()
            .with_url(&ch_url)
            .with_user(&ch_user)
            .with_password(&ch_password);
        ch.query(&format!("CREATE DATABASE {}", identifier(&database)))
            .execute()
            .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE SCHEMA {}",
            identifier(&schema)
        )))
        .execute(&pg)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DOMAIN {}.amount_domain AS integer",
            identifier(&schema)
        )))
        .execute(&pg)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TYPE {}.mood AS ENUM ('happy', 'sad')",
            identifier(&schema)
        )))
        .execute(&pg)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TYPE {}.geometry AS (x double precision, y double precision)",
            identifier(&schema)
        )))
        .execute(&pg)
        .await?;
        if postgis {
            sqlx::query("CREATE SCHEMA spatial").execute(&pg).await?;
            sqlx::query("CREATE EXTENSION postgis WITH SCHEMA spatial")
                .execute(&pg)
                .await?;
            let version: String =
                sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'postgis'")
                    .fetch_one(&pg)
                    .await?;
            assert!(
                version.starts_with("3.6."),
                "expected PostGIS 3.6, got {version}"
            );
        }
        let version: String = sqlx::query_scalar("SHOW server_version")
            .fetch_one(&pg)
            .await?;
        assert!(
            version.starts_with("14."),
            "expected PostgreSQL 14, got {version}"
        );
        let ch_version: String = ch.query("SELECT version()").fetch_one().await?;
        assert!(
            ch_version.starts_with("26.8."),
            "expected ClickHouse 26.8 LTS, got {ch_version}"
        );
        eprintln!(
            "integration servers: PostgreSQL {version}, ClickHouse {ch_version}, PostGIS={postgis}"
        );
        let fixtures: Vec<Case> = serde_json::from_str(include_str!("fixtures/cases.json"))?;
        let mut cases = Vec::new();
        for case in fixtures {
            if !case.postgis || postgis {
                cases.push(case);
            }
        }
        Ok(Self {
            admin,
            pg,
            ch,
            pg_url,
            ch_url,
            ch_user,
            ch_password,
            database,
            schema,
            cases,
            files: tempfile::tempdir()?,
        })
    }

    async fn fixtures(&self) -> TestResult {
        for case in &self.cases {
            let pg_table = format!("{}.{}", identifier(&self.schema), identifier(&case.name));
            let columns = case
                .pg_columns
                .replace("{schema}", &identifier(&self.schema));
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TABLE {pg_table} ({columns}) {}",
                case.pg_suffix
            )))
            .execute(&self.pg)
            .await
            .map_err(|error| format!("fixture {}: PostgreSQL CREATE TABLE: {error}", case.name))?;
            for statement in &case.pg_after {
                sqlx::query(sqlx::AssertSqlSafe(statement.replace("{table}", &pg_table)))
                    .execute(&self.pg)
                    .await
                    .map_err(|error| {
                        format!("fixture {}: PostgreSQL alteration: {error}", case.name)
                    })?;
            }
            let columns = &case.ch_columns;
            self.ch
                .query(&format!(
                    "CREATE TABLE {}.{} ({columns}) ENGINE = MergeTree ORDER BY tuple()",
                    identifier(&self.database),
                    identifier(&case.name)
                ))
                .execute()
                .await
                .map_err(|error| {
                    format!("fixture {}: ClickHouse CREATE TABLE: {error}", case.name)
                })?;
        }
        Ok(())
    }

    async fn config(
        &self,
        name: &str,
        cases: &[&Case],
        ca_cert: Option<&Path>,
    ) -> TestResult<PathBuf> {
        let database = format!("{}__{name}", self.database);
        self.ch
            .query(&format!("CREATE DATABASE {}", identifier(&database)))
            .execute()
            .await?;
        let mut ignored = BTreeSet::new();
        for case in cases {
            self.ch
                .query(&format!(
                    "CREATE TABLE {}.{} AS {}.{}",
                    identifier(&database),
                    identifier(&case.name),
                    identifier(&self.database),
                    identifier(&case.name)
                ))
                .execute()
                .await?;
            for column in &case.ignore {
                ignored.insert(column);
            }
        }
        let mut config = json!({
            "config_version": 1,
            "postgres": { "url_env": "PG_URL", "schema": self.schema },
            "clickhouse": { "url_env": "CH_URL", "database": database, "user_env": "CH_USER", "password_env": "CH_PASSWORD" },
            "comparison": { "policy": "pg-clickhouse-v1", "connect_timeout_seconds": 5, "query_timeout_seconds": 15, "ignore_target_columns": ignored }
        });
        if let Some(ca_cert) = ca_cert {
            config["clickhouse"]["ca_cert"] = json!(ca_cert);
        }
        let path = self.files.path().join(format!("{name}.toml"));
        fs::write(&path, toml::to_string(&config)?)?;
        Ok(path)
    }

    fn run(&self, config: &Path, format: &str, overrides: &[(&str, &str)]) -> TestResult<Output> {
        self.run_command("diff", config, format, overrides)
    }

    fn run_command(
        &self,
        command_name: &str,
        config: &Path,
        format: &str,
        overrides: &[(&str, &str)],
    ) -> TestResult<Output> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sync-pg2ch"));
        command
            .args([command_name, "--config"])
            .arg(config)
            .args(["--format", format])
            .env("PG_URL", &self.pg_url)
            .env("CH_URL", &self.ch_url)
            .env("CH_USER", &self.ch_user)
            .env("CH_PASSWORD", &self.ch_password);
        for (name, value) in overrides {
            command.env(name, value);
        }
        Ok(command.output()?)
    }

    fn report(&self, name: &str, output: &Output, status: &str) -> TestResult<Value> {
        if let Ok(directory) = std::env::var("TEST_ARTIFACTS") {
            fs::create_dir_all(&directory)?;
            fs::write(
                Path::new(&directory).join(format!("{name}.json")),
                &output.stdout,
            )?;
            fs::write(
                Path::new(&directory).join(format!("{name}.stderr")),
                &output.stderr,
            )?;
        }
        assert_eq!(
            output.status.code(),
            Some(exit_code(status)),
            "{name}: stderr={} stdout={}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["report_version"], 1, "{name}");
        assert_eq!(report["policy"], "pg-clickhouse-v1", "{name}");
        assert_eq!(report["status"], status, "{name}");
        assert_eq!(report["complete"], status != "unknown", "{name}");
        for secret in [&self.ch_password, &self.pg_url] {
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains(secret),
                "secret leaked to stdout"
            );
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains(secret),
                "secret leaked to stderr"
            );
        }
        Ok(report)
    }

    async fn cleanup(self) -> TestResult {
        let databases: Vec<String> = self
            .ch
            .query("SELECT name FROM system.databases WHERE name = ? OR startsWith(name, ?)")
            .bind(&self.database)
            .bind(format!("{}__", self.database))
            .fetch_all()
            .await?;
        for database in databases {
            self.ch
                .query(&format!("DROP DATABASE {}", identifier(&database)))
                .execute()
                .await?;
        }
        self.pg.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {}",
            identifier(&self.database)
        )))
        .execute(&self.admin)
        .await?;
        self.admin.close().await;
        Ok(())
    }
}

fn assert_cases(report: &Value, cases: &[&Case]) {
    let tables = report["tables"]
        .as_array()
        .expect("tables must be an array");
    assert_eq!(tables.len(), cases.len());
    let mut ignored = BTreeSet::new();
    for case in cases {
        for column in &case.ignore {
            ignored.insert(column);
        }
    }
    let mut previous = "";
    for table in tables {
        let name = table["target"]["table"]
            .as_str()
            .expect("target table name");
        assert!(
            previous <= name,
            "table report is not sorted: {previous}, {name}"
        );
        previous = name;
        let mut case = None;
        for candidate in cases {
            if candidate.name == name {
                case = Some(*candidate);
                break;
            }
        }
        let case = case.expect("unexpected table in report");
        assert_eq!(table["status"], case.status, "{}: {table}", case.name);
        assert_eq!(
            table["ignored_target_columns"],
            json!(ignored),
            "{}",
            case.name
        );
        let findings = table["findings"]
            .as_array()
            .expect("findings must be an array");
        if let Some(code) = &case.code {
            let mut found = false;
            for finding in findings {
                if finding["code"] == *code {
                    found = true;
                }
            }
            assert!(found, "{}: missing finding {code}: {table}", case.name);
        }
        if let Some(expected) = &case.geo {
            let mut source = None;
            for finding in findings {
                if finding["column"] == "v" && !finding["source"].is_null() {
                    source = Some(&finding["source"]["data_type"]);
                    break;
                }
            }
            let source = source.expect("geo source metadata must be present, even on a match");
            let geo = &source["geo"];
            for (key, value) in expected.as_object().expect("expected geo object") {
                assert_eq!(geo[key], *value, "{} geo.{key}: {source}", case.name);
            }
            if case.postgis {
                assert_eq!(source["extension"], "postgis");
                assert_eq!(source["schema"], "spatial");
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn services_compatibility_matrix() -> TestResult {
    let environment = Environment::create().await?;
    environment.fixtures().await?;
    for status in ["match", "compatible", "mismatch", "unknown"] {
        let mut cases = Vec::new();
        for case in &environment.cases {
            if case.status == status {
                cases.push(case);
            }
        }
        assert!(!cases.is_empty(), "empty fixture group {status}");
        let config = environment.config(status, &cases, None).await?;
        let output = environment.run(&config, "json", &[])?;
        let report = environment.report(status, &output, status)?;
        assert_cases(&report, &cases);
        let repeated = environment.run(&config, "json", &[])?;
        let repeated: Value = serde_json::from_slice(&repeated.stdout)?;
        assert_eq!(
            report["tables"], repeated["tables"],
            "findings changed without a schema change"
        );
    }
    let mut mixed = Vec::new();
    for status in ["match", "mismatch", "unknown"] {
        for case in &environment.cases {
            if case.status == status {
                mixed.push(case);
                break;
            }
        }
    }
    let config = environment.config("mixed", &mixed, None).await?;
    let output = environment.run(&config, "json", &[])?;
    let report = environment.report("mixed", &output, "unknown")?;
    assert_cases(&report, &mixed);
    cli_errors(&environment).await?;
    automatic_discovery(&environment).await?;
    peerdb_discovery(&environment).await?;
    peerdb_mapping_races(&environment).await?;
    metadata_only(&environment).await?;
    numeric_boundaries(&environment).await?;
    catalog_failures(&environment).await?;
    ddl::checks(&environment).await?;
    environment.cleanup().await
}

struct MockReply {
    query: &'static str,
    body: Vec<u8>,
    delay: Duration,
    before_reply: Option<Box<dyn FnOnce() -> TestResult + Send>>,
}

impl MockReply {
    fn row(query: &'static str, fields: &[&str]) -> Self {
        let mut body = Vec::new();
        for field in fields {
            let length =
                u8::try_from(field.len()).expect("mock strings are shorter than 128 bytes");
            assert!(length < 128);
            body.push(length);
            body.extend_from_slice(field.as_bytes());
        }
        Self {
            query,
            body,
            delay: Duration::ZERO,
            before_reply: None,
        }
    }

    fn grant(query: &'static str) -> Self {
        Self {
            query,
            body: vec![1],
            delay: Duration::ZERO,
            before_reply: None,
        }
    }

    fn with_hook(mut self, hook: impl FnOnce() -> TestResult + Send + 'static) -> Self {
        self.before_reply = Some(Box::new(hook));
        self
    }
}

fn mock_catalog(replies: Vec<MockReply>) -> TestResult<(String, thread::JoinHandle<TestResult>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    listener.set_nonblocking(true)?;
    let server = thread::spawn(move || -> TestResult {
        for MockReply {
            query,
            body,
            delay,
            before_reply,
        } in replies
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return Err(format!("mock did not receive query {query}").into());
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut request = BufReader::new(stream.try_clone()?);
            let mut line = String::new();
            request.read_line(&mut line)?;
            assert!(line.starts_with("POST "), "unexpected mock HTTP request");
            let mut length = None;
            loop {
                line.clear();
                if request.read_line(&mut line)? == 0 {
                    return Err("mock received incomplete HTTP headers".into());
                }
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>()?);
                }
            }
            let length = length.ok_or("mock requires Content-Length for SQL query")?;
            assert!(length < 65536, "unexpectedly large metadata query");
            let mut sql = vec![0; length];
            request.read_exact(&mut sql)?;
            let sql = String::from_utf8(sql)?;
            assert!(sql.starts_with(query), "expected {query}, received {sql}");
            if let Some(hook) = before_reply {
                hook()?;
            }
            if !delay.is_zero() {
                thread::sleep(delay);
                continue;
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )?;
            stream.write_all(&body)?;
            stream.flush()?;
        }
        Ok(())
    });
    Ok((endpoint, server))
}

async fn catalog_failures(environment: &Environment) -> TestResult {
    let mut source = None;
    for case in &environment.cases {
        if case.name == "bigint" {
            source = Some(case);
            break;
        }
    }
    let source = source.expect("bigint fixture is required by catalog failure tests");
    let config = environment.config("catalog-race", &[source], None).await?;
    let (endpoint, server) = mock_catalog(vec![
        MockReply::row("SELECT version()", &["26.8.36.3"]),
        MockReply::grant("CHECK GRANT SHOW TABLES"),
        MockReply::grant("CHECK GRANT SHOW COLUMNS"),
        MockReply::row("SELECT name FROM system.databases", &["catalog-race"]),
        MockReply::row("SELECT name FROM system.tables", &["bigint"]),
        MockReply::row("SELECT engine FROM system.tables", &["MergeTree"]),
        MockReply::row(
            "SELECT name, type, default_kind FROM system.columns",
            &["v", "Int32", ""],
        ),
        MockReply::row("SELECT engine FROM system.tables", &["MergeTree"]),
        MockReply::row(
            "SELECT name, type, default_kind FROM system.columns",
            &["v", "Int64", ""],
        ),
        MockReply::row("SELECT name FROM system.databases", &["catalog-race"]),
        MockReply::row("SELECT name FROM system.tables", &["bigint"]),
    ])?;
    let output = environment.run(&config, "json", &[("CH_URL", &endpoint)])?;
    server
        .join()
        .map_err(|_| "mock catalog server panicked")??;
    let report = environment.report("catalog-race", &output, "unknown")?;
    let findings = report["tables"][0]["findings"]
        .as_array()
        .expect("findings array");
    let mut observed_race = false;
    let mut preserved_diff = false;
    for finding in findings {
        if finding["code"] == "schema_changed_during_check" {
            observed_race = true;
        }
        if finding["status"] == "mismatch" && finding["target"]["data_type"] == "Int32" {
            preserved_diff = true;
        }
    }
    assert!(observed_race, "missing DDL race finding: {report}");
    assert!(preserved_diff, "original diff was discarded: {report}");
    for names in [vec!["bigint", "new_table"], Vec::new()] {
        let (endpoint, server) = mock_catalog(vec![
            MockReply::row("SELECT version()", &["26.8.36.3"]),
            MockReply::grant("CHECK GRANT SHOW TABLES"),
            MockReply::grant("CHECK GRANT SHOW COLUMNS"),
            MockReply::row("SELECT name FROM system.databases", &["catalog-race"]),
            MockReply::row("SELECT name FROM system.tables", &["bigint"]),
            MockReply::row("SELECT engine FROM system.tables", &["MergeTree"]),
            MockReply::row(
                "SELECT name, type, default_kind FROM system.columns",
                &["v", "Int64", ""],
            ),
            MockReply::row("SELECT engine FROM system.tables", &["MergeTree"]),
            MockReply::row(
                "SELECT name, type, default_kind FROM system.columns",
                &["v", "Int64", ""],
            ),
            MockReply::row("SELECT name FROM system.databases", &["catalog-race"]),
            MockReply::row("SELECT name FROM system.tables", &names),
        ])?;
        let output = environment.run(&config, "json", &[("CH_URL", &endpoint)])?;
        server
            .join()
            .map_err(|_| "mock discovery server panicked")??;
        let report = environment.report("discovery-race", &output, "unknown")?;
        assert_eq!(
            report["errors"][0]["code"],
            "table_set_changed_during_check"
        );
        assert_eq!(report["tables"][0]["status"], "match");
    }
    let mut value: toml::Value = toml::from_str(&fs::read_to_string(&config)?)?;
    value["comparison"]["query_timeout_seconds"] = toml::Value::Integer(1);
    fs::write(&config, toml::to_string(&value)?)?;
    let (endpoint, server) = mock_catalog(vec![MockReply {
        query: "SELECT version()",
        body: Vec::new(),
        delay: Duration::from_millis(1500),
        before_reply: None,
    }])?;
    let output = environment.run(&config, "json", &[("CH_URL", &endpoint)])?;
    server
        .join()
        .map_err(|_| "mock timeout server panicked")??;
    let report = environment.report("catalog-timeout", &output, "unknown")?;
    let mut observed_timeout = false;
    for error in report["errors"].as_array().expect("errors array") {
        if error["code"] == "catalog_timeout" {
            observed_timeout = true;
        }
    }
    assert!(observed_timeout, "missing catalog timeout error: {report}");
    Ok(())
}

async fn cli_errors(environment: &Environment) -> TestResult {
    let cases = [&environment.cases[0]];
    let config = environment.config("single", &cases, None).await?;
    let output = environment.run(&config, "text", &[])?;
    assert_eq!(output.status.code(), Some(0));
    assert!(!output.stdout.is_empty());
    let output = environment.run(&config, "json", &[("CH_PASSWORD", "wrong-password-marker")])?;
    environment.report("bad-credentials", &output, "unknown")?;
    assert!(!String::from_utf8_lossy(&output.stdout).contains("wrong-password-marker"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("wrong-password-marker"));
    let empty = environment.config("empty", &[], None).await?;
    let report = environment.report("empty", &environment.run(&empty, "json", &[])?, "unknown")?;
    assert_eq!(report["errors"][0]["code"], "no_tables_discovered");
    let mut missing_db: toml::Value = toml::from_str(&fs::read_to_string(&empty)?)?;
    missing_db["clickhouse"]["database"] =
        toml::Value::String(format!("{}__does_not_exist", environment.database));
    fs::write(&empty, toml::to_string(&missing_db)?)?;
    let report = environment.report(
        "missing-database",
        &environment.run(&empty, "json", &[])?,
        "unknown",
    )?;
    assert_eq!(report["errors"][0]["code"], "clickhouse_database_missing");
    let mut config_value: toml::Value = toml::from_str(&fs::read_to_string(&config)?)?;
    config_value["comparison"]["ignore_target_columns"] =
        toml::Value::Array(vec![toml::Value::String("v".to_owned())]);
    let conflict = environment.files.path().join("ignore-conflict.toml");
    fs::write(&conflict, toml::to_string(&config_value)?)?;
    assert_eq!(
        environment.run(&conflict, "json", &[])?.status.code(),
        Some(2)
    );
    let missing = Case {
        name: "missing_table".to_owned(),
        pg_columns: "v integer NOT NULL".to_owned(),
        ch_columns: "v Int32".to_owned(),
        status: "mismatch".to_owned(),
        postgis: false,
        pg_suffix: String::new(),
        pg_after: Vec::new(),
        ignore: Vec::new(),
        code: None,
        geo: None,
    };
    environment
        .ch
        .query(&format!(
            "CREATE TABLE {}.{} (v Int32) ENGINE=MergeTree ORDER BY tuple()",
            identifier(&environment.database),
            identifier(&missing.name)
        ))
        .execute()
        .await?;
    let config = environment
        .config("missing", &[cases[0], &missing], None)
        .await?;
    let report = environment.report(
        "missing",
        &environment.run(&config, "json", &[])?,
        "mismatch",
    )?;
    assert_cases(&report, &[cases[0], &missing]);
    Ok(())
}

async fn automatic_discovery(environment: &Environment) -> TestResult {
    let config = environment
        .config("automatic", &[&environment.cases[0]], None)
        .await?;
    let contents = fs::read_to_string(&config)?;
    let settings: toml::Value = toml::from_str(&contents)?;
    assert!(settings.get("tables").is_none());
    let database = settings["clickhouse"]["database"]
        .as_str()
        .expect("discovery database");
    let output = environment.run(&config, "json", &[])?;
    let report = environment.report("automatic-before", &output, "match")?;
    assert_eq!(report["tables"].as_array().expect("tables").len(), 1);

    let name = "event.\"Юникод";
    let pg_table = format!("{}.{}", identifier(&environment.schema), identifier(name));
    let ch_table = format!("{}.{}", identifier(database), identifier(name));
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE {pg_table} (v integer NOT NULL)"
    )))
    .execute(&environment.pg)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE {}.pg_only (v integer NOT NULL)",
        identifier(&environment.schema)
    )))
    .execute(&environment.pg)
    .await?;
    environment
        .ch
        .query(&format!(
            "CREATE TABLE {ch_table} (v Int32) ENGINE=MergeTree ORDER BY tuple()"
        ))
        .execute()
        .await?;
    let output = environment.run(&config, "json", &[])?;
    let report = environment.report("automatic-after", &output, "match")?;
    assert_eq!(report["tables"].as_array().expect("tables").len(), 2);
    assert_eq!(report["tables"][0]["source"]["table"], name);
    assert_eq!(report["tables"][0]["source"]["schema"], environment.schema);
    assert_eq!(report["tables"][0]["target"]["database"], database);
    assert_eq!(contents, fs::read_to_string(&config)?);

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE {pg_table} SET SCHEMA public"
    )))
    .execute(&environment.pg)
    .await?;
    let report = environment.report(
        "automatic-wrong-source-schema",
        &environment.run(&config, "json", &[])?,
        "mismatch",
    )?;
    assert_eq!(
        report["tables"][0]["findings"][0]["code"],
        "source_table_missing"
    );

    environment
        .ch
        .query(&format!(
            "CREATE VIEW {}.unsupported_view AS SELECT toInt32(1) AS v",
            identifier(database)
        ))
        .execute()
        .await?;
    let report = environment.report(
        "automatic-unsupported-object",
        &environment.run(&config, "json", &[])?,
        "unknown",
    )?;
    assert_eq!(report["tables"].as_array().expect("tables").len(), 3);
    assert_eq!(report["tables"][2]["status"], "unknown");
    Ok(())
}

async fn peerdb_discovery(environment: &Environment) -> TestResult {
    for (index, schema) in ["public", "Sales_eu.\"Ю"].into_iter().enumerate() {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            identifier(schema),
        )))
        .execute(&environment.pg)
        .await?;
        let config = environment
            .config(&format!("peerdb-{index}"), &[], None)
            .await?;
        let mut settings: toml::Value = toml::from_str(&fs::read_to_string(&config)?)?;
        settings["postgres"]["schema"] = toml::Value::String(schema.to_owned());
        fs::write(&config, toml::to_string(&settings)?)?;
        let database = settings["clickhouse"]["database"]
            .as_str()
            .expect("database");
        let prefixed_literal = format!("{schema}_literal_prefixed");
        let pairs = [
            ("literal_only", "literal_only".to_owned()),
            ("orders_items", format!("{schema}_orders_items")),
            ("event.\"Ю", format!("{schema}_event.\"Ю")),
            (prefixed_literal.as_str(), prefixed_literal.clone()),
            ("_leading", format!("{schema}__leading")),
        ];
        for (source, target) in &pairs {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TABLE {}.{} (v integer NOT NULL)",
                identifier(schema),
                identifier(source),
            )))
            .execute(&environment.pg)
            .await?;
            environment
                .ch
                .query(&format!(
                    "CREATE TABLE {}.{} (v Int32) ENGINE=MergeTree ORDER BY tuple()",
                    identifier(database),
                    identifier(target),
                ))
                .execute()
                .await?;
        }
        let report = environment.report(
            "peerdb-match",
            &environment.run(&config, "json", &[])?,
            "match",
        )?;
        assert_eq!(
            report["tables"].as_array().expect("tables").len(),
            pairs.len()
        );
        for (source, target) in &pairs {
            let table = table_report(&report, target);
            assert_eq!(
                table["source"],
                json!({ "schema": schema, "table": source })
            );
            assert_eq!(table["source_candidates"], json!([table["source"]]));
        }

        let collision = format!("{schema}_orders_items");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {}.{} (v integer NOT NULL)",
            identifier(schema),
            identifier(&collision),
        )))
        .execute(&environment.pg)
        .await?;
        let report = environment.report(
            "peerdb-ambiguous",
            &environment.run(&config, "json", &[])?,
            "unknown",
        )?;
        let table = table_report(&report, &collision);
        assert!(table["source"].is_null());
        assert_eq!(table["findings"][0]["code"], "ambiguous_source_table");
        let mut candidates = ["orders_items", collision.as_str()];
        candidates.sort();
        assert_eq!(
            table["source_candidates"],
            json!([
                { "schema": schema, "table": candidates[0] },
                { "schema": schema, "table": candidates[1] },
            ])
        );
        assert_eq!(report["counts"]["match"], pairs.len() - 1);
        let output = environment.run(&config, "text", &[])?;
        assert_eq!(output.status.code(), Some(2));
        let text = String::from_utf8(output.stdout)?;
        assert!(text.contains("(unresolved source)"));
        assert_eq!(text.matches("source candidate:").count(), 2);
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE {}.orders_items",
            identifier(schema),
        )))
        .execute(&environment.pg)
        .await?;
        let report = environment.report(
            "peerdb-literal-prefix",
            &environment.run(&config, "json", &[])?,
            "match",
        )?;
        assert_eq!(
            table_report(&report, &collision)["source"]["table"],
            collision
        );

        for target in [
            format!("{schema}_absent"),
            "custom_rename".to_owned(),
            "Public_literal_only".to_owned(),
        ] {
            environment
                .ch
                .query(&format!(
                    "CREATE TABLE {}.{} (v Int32) ENGINE=MergeTree ORDER BY tuple()",
                    identifier(database),
                    identifier(&target),
                ))
                .execute()
                .await?;
        }
        let report = environment.report(
            "peerdb-missing",
            &environment.run(&config, "json", &[])?,
            "mismatch",
        )?;
        for target in [
            format!("{schema}_absent"),
            "custom_rename".to_owned(),
            "Public_literal_only".to_owned(),
        ] {
            let table = table_report(&report, &target);
            assert!(table["source"].is_null());
            assert_eq!(table["source_candidates"], json!([]));
            assert_eq!(table["findings"][0]["code"], "source_table_missing");
        }
    }
    Ok(())
}

async fn peerdb_mapping_races(environment: &Environment) -> TestResult {
    let config = environment.config("peerdb-races", &[], None).await?;
    let target = format!("{}_peerdb_race", environment.schema);
    let source = format!("{}.peerdb_race", identifier(&environment.schema));
    let literal = format!(
        "{}.{}",
        identifier(&environment.schema),
        identifier(&target)
    );
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE {source} (v bigint NOT NULL)"
    )))
    .execute(&environment.pg)
    .await?;
    for (name, ddl, initial_code) in [
        (
            "collision-added",
            format!("CREATE TABLE {literal} (v bigint NOT NULL)"),
            "type_mismatch",
        ),
        (
            "collision-removed",
            format!("DROP TABLE {literal}"),
            "ambiguous_source_table",
        ),
        (
            "source-removed",
            format!("DROP TABLE {source}"),
            "type_mismatch",
        ),
        (
            "source-added",
            format!("CREATE TABLE {source} (v bigint NOT NULL)"),
            "source_table_missing",
        ),
    ] {
        let url = environment.pg_url.clone();
        let barrier = MockReply::row("SELECT engine FROM system.tables", &["MergeTree"]).with_hook(
            move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(async {
                    let pool = sqlx::PgPool::connect(&url).await?;
                    sqlx::query(sqlx::AssertSqlSafe(ddl)).execute(&pool).await?;
                    pool.close().await;
                    Ok(())
                })
            },
        );
        let (endpoint, server) = mock_catalog(vec![
            MockReply::row("SELECT version()", &["26.8.36.3"]),
            MockReply::grant("CHECK GRANT SHOW TABLES"),
            MockReply::grant("CHECK GRANT SHOW COLUMNS"),
            MockReply::row("SELECT name FROM system.databases", &["peerdb-races"]),
            MockReply::row("SELECT name FROM system.tables", &[&target, "zz_barrier"]),
            MockReply::row("SELECT engine FROM system.tables", &["MergeTree"]),
            MockReply::row(
                "SELECT name, type, default_kind FROM system.columns",
                &["v", "Int32", ""],
            ),
            barrier,
            MockReply::row(
                "SELECT name, type, default_kind FROM system.columns",
                &["v", "Int32", ""],
            ),
            MockReply::row("SELECT engine FROM system.tables", &["MergeTree"]),
            MockReply::row(
                "SELECT name, type, default_kind FROM system.columns",
                &["v", "Int32", ""],
            ),
            MockReply::row("SELECT engine FROM system.tables", &["MergeTree"]),
            MockReply::row(
                "SELECT name, type, default_kind FROM system.columns",
                &["v", "Int32", ""],
            ),
            MockReply::row("SELECT name FROM system.databases", &["peerdb-races"]),
            MockReply::row("SELECT name FROM system.tables", &[&target, "zz_barrier"]),
        ])?;
        let output = environment.run(&config, "json", &[("CH_URL", &endpoint)])?;
        server
            .join()
            .map_err(|_| "mock mapping race server panicked")??;
        let report = environment.report(name, &output, "unknown")?;
        let table = table_report(&report, &target);
        let mut codes = BTreeSet::new();
        for finding in table["findings"].as_array().expect("findings") {
            codes.insert(finding["code"].as_str().expect("code"));
        }
        assert!(
            codes.contains("source_mapping_changed_during_check"),
            "{report}"
        );
        assert!(
            codes.contains(initial_code),
            "original finding was discarded: {report}"
        );
    }
    Ok(())
}

fn table_report<'a>(report: &'a Value, target: &str) -> &'a Value {
    for table in report["tables"].as_array().expect("tables") {
        if table["target"]["table"] == target {
            return table;
        }
    }
    panic!("target {target:?} missing from report: {report}");
}

async fn metadata_only(environment: &Environment) -> TestResult {
    let role = format!("{}_reader", environment.database);
    let password = "catalog-only-test-password";
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {} LOGIN PASSWORD '{password}'",
        identifier(&role)
    )))
    .execute(&environment.admin)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "GRANT CONNECT ON DATABASE {} TO {}",
        identifier(&environment.database),
        identifier(&role)
    )))
    .execute(&environment.admin)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "GRANT USAGE ON SCHEMA {} TO {}",
        identifier(&environment.schema),
        identifier(&role)
    )))
    .execute(&environment.pg)
    .await?;
    environment
        .ch
        .query(&format!(
            "CREATE USER {} IDENTIFIED WITH sha256_password BY '{password}'",
            identifier(&role)
        ))
        .execute()
        .await?;
    environment
        .ch
        .query(&format!(
            "GRANT SHOW TABLES, SHOW COLUMNS ON *.* TO {}",
            identifier(&role)
        ))
        .execute()
        .await?;
    let mut pg_url = Url::parse(&environment.pg_url)?;
    pg_url
        .set_username(&role)
        .map_err(|()| "could not replace test username")?;
    pg_url
        .set_password(Some(password))
        .map_err(|()| "could not replace test password")?;
    let pg_url = pg_url.to_string();
    let pg = PgPool::connect(&pg_url).await?;
    let ch = Client::default()
        .with_url(&environment.ch_url)
        .with_user(&role)
        .with_password(password);
    let table = &environment.cases[0].name;
    assert!(
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT * FROM {}.{}",
            identifier(&environment.schema),
            identifier(table)
        )))
        .fetch_all(&pg)
        .await
        .is_err()
    );
    assert!(
        ch.query(&format!(
            "SELECT * FROM {}.{}",
            identifier(&environment.database),
            identifier(table)
        ))
        .fetch_all::<i32>()
        .await
        .is_err()
    );
    assert!(
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {}.{} ADD COLUMN forbidden integer",
            identifier(&environment.schema),
            identifier(table)
        )))
        .execute(&pg)
        .await
        .is_err()
    );
    assert!(
        ch.query(&format!(
            "ALTER TABLE {}.{} ADD COLUMN forbidden Int32",
            identifier(&environment.database),
            identifier(table)
        ))
        .execute()
        .await
        .is_err()
    );
    let config = environment
        .config("metadata-only", &[&environment.cases[0]], None)
        .await?;
    let output = environment.run(
        &config,
        "json",
        &[
            ("PG_URL", &pg_url),
            ("CH_USER", &role),
            ("CH_PASSWORD", password),
        ],
    )?;
    environment.report("metadata-only", &output, "match")?;
    environment
        .ch
        .query(&format!(
            "REVOKE SHOW TABLES, SHOW COLUMNS ON *.* FROM {}",
            identifier(&role)
        ))
        .execute()
        .await?;
    let output = environment.run(
        &config,
        "json",
        &[
            ("PG_URL", &pg_url),
            ("CH_USER", &role),
            ("CH_PASSWORD", password),
        ],
    )?;
    environment.report("metadata-hidden", &output, "unknown")?;
    pg.close().await;
    environment
        .ch
        .query(&format!("DROP USER {}", identifier(&role)))
        .execute()
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP OWNED BY {}",
        identifier(&role)
    )))
    .execute(&environment.pg)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP ROLE {}",
        identifier(&role)
    )))
    .execute(&environment.admin)
    .await?;
    Ok(())
}

async fn numeric_boundaries(environment: &Environment) -> TestResult {
    let table = format!(
        "{}.{}",
        identifier(&environment.database),
        identifier("numeric_boundaries")
    );
    environment.ch.query(&format!("CREATE TABLE {table} (i Int64, d Decimal64(4), f Float64) ENGINE = MergeTree ORDER BY tuple()"))
        .execute().await?;
    environment.ch.query(&format!("INSERT INTO {table} VALUES (-32768, -99999999.9900, -3.4028234663852886e38), (32767, 99999999.9900, 3.4028234663852886e38)"))
        .execute().await?;
    let values: Vec<(i64, String)> = environment
        .ch
        .query(&format!("SELECT i, toString(d) FROM {table} ORDER BY i"))
        .fetch_all()
        .await?;
    assert_eq!(
        values,
        vec![
            (-32768, "-99999999.99".to_owned()),
            (32767, "99999999.99".to_owned())
        ]
    );
    let source: String = sqlx::query("SELECT (-99999999.99::numeric(10,2))::text AS value")
        .fetch_one(&environment.pg)
        .await?
        .try_get("value")?;
    assert_eq!(source, values[0].1);
    let floats: Vec<f64> = environment
        .ch
        .query(&format!("SELECT f FROM {table} ORDER BY i"))
        .fetch_all()
        .await?;
    assert_eq!(floats, vec![-f64::from(f32::MAX), f64::from(f32::MAX)]);
    let source_float: f64 =
        sqlx::query_scalar("SELECT ('3.4028234663852886e38'::real)::double precision")
            .fetch_one(&environment.pg)
            .await?;
    assert_eq!(source_float, floats[1]);
    temporal_boundaries(environment).await?;
    Ok(())
}

async fn temporal_boundaries(environment: &Environment) -> TestResult {
    for precision in 0..=6 {
        for value in ["1900-01-01 00:00:00.000000", "2299-12-31 23:59:59.000000"] {
            let query =
                format!("SELECT to_char($1::timestamp({precision}), 'YYYY-MM-DD HH24:MI:SS.US')");
            let source: String = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
                .bind(value)
                .fetch_one(&environment.pg)
                .await?;
            let query = format!(
                "SELECT toString(CAST(toDateTime64(?, {precision}, 'UTC') AS DateTime64(6, 'UTC')))"
            );
            let target: String = environment.ch.query(&query).bind(value).fetch_one().await?;
            assert_eq!(
                source, target,
                "DateTime64({precision}) widening boundary {value}"
            );
        }
    }
    for value in ["1900-01-01", "2299-12-31"] {
        let source: String = sqlx::query_scalar("SELECT ($1::date)::text")
            .bind(value)
            .fetch_one(&environment.pg)
            .await?;
        let target: String = environment
            .ch
            .query("SELECT toString(toDate32(?))")
            .bind(value)
            .fetch_one()
            .await?;
        assert_eq!(source, target);
    }
    let target: String = environment.ch.query("SELECT toString(CAST(toDateTime64('2299-12-31 23:59:59.999', 3, 'UTC') AS DateTime64(6, 'UTC')))").fetch_one().await?;
    assert_eq!(target, "2299-12-31 23:59:59.999000");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires TEST_PG_TLS_URL, TEST_CH_TLS_URL, TEST_CH_CA_CERT; CI runs this explicitly"]
async fn tls_certificate_validation() -> TestResult {
    let environment = Environment::create().await?;
    environment.fixtures().await?;
    let ca_cert = PathBuf::from(required_env("TEST_CH_CA_CERT")?);
    let mut pg_url = Url::parse(&required_env("TEST_PG_TLS_URL")?)?;
    pg_url.set_path(&environment.database);
    let pg_url = pg_url.to_string();
    let ch_url = required_env("TEST_CH_TLS_URL")?;
    let config = environment
        .config("tls", &[&environment.cases[0]], Some(&ca_cert))
        .await?;
    let output = environment.run(&config, "json", &[("PG_URL", &pg_url), ("CH_URL", &ch_url)])?;
    environment.report("tls-trusted", &output, "match")?;
    let config = environment
        .config("tls-untrusted-ch", &[&environment.cases[0]], None)
        .await?;
    let output = environment.run(&config, "json", &[("PG_URL", &pg_url), ("CH_URL", &ch_url)])?;
    environment.report("tls-untrusted-ch", &output, "unknown")?;
    let mut untrusted_pg = Url::parse(&pg_url)?;
    untrusted_pg.set_query(Some("sslmode=verify-full"));
    let config = environment
        .config("tls-untrusted-pg", &[&environment.cases[0]], Some(&ca_cert))
        .await?;
    let output = environment.run(
        &config,
        "json",
        &[("PG_URL", untrusted_pg.as_str()), ("CH_URL", &ch_url)],
    )?;
    environment.report("tls-untrusted-pg", &output, "unknown")?;
    let mut wrong_host_pg = Url::parse(&pg_url)?;
    wrong_host_pg.set_host(Some("127.0.0.1"))?;
    let output = environment.run(
        &config,
        "json",
        &[("PG_URL", wrong_host_pg.as_str()), ("CH_URL", &ch_url)],
    )?;
    environment.report("tls-hostname-pg", &output, "unknown")?;
    let mut wrong_host_ch = Url::parse(&ch_url)?;
    wrong_host_ch.set_host(Some("127.0.0.1"))?;
    let output = environment.run(
        &config,
        "json",
        &[("PG_URL", &pg_url), ("CH_URL", wrong_host_ch.as_str())],
    )?;
    environment.report("tls-hostname-ch", &output, "unknown")?;
    ddl::tls_checks(&environment, &pg_url, &ch_url, &ca_cert).await?;
    environment.cleanup().await
}
