use super::*;
use std::net::TcpStream;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

pub(super) async fn checks(environment: &Environment) -> TestResult {
    success(environment).await?;
    peerdb_layout(environment).await?;
    connection_timezone(environment).await?;
    blockers(environment).await?;
    permissions(environment).await?;
    races_and_dependencies(environment).await?;
    runtime_failures(environment).await?;
    Ok(())
}

fn report(name: &str, output: &Output, code: i32, status: &str) -> TestResult<Value> {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{name}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["report_version"], 1);
    assert_eq!(value["status"], status, "{name}: {value}");
    if let Ok(directory) = std::env::var("TEST_ARTIFACTS") {
        fs::write(
            Path::new(&directory).join(format!("ddl-{name}.json")),
            &output.stdout,
        )?;
    }
    Ok(value)
}

async fn config(environment: &Environment, name: &str) -> TestResult<(PathBuf, String)> {
    let path = environment.config(name, &[], None).await?;
    let value: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
    let database = value["clickhouse"]["database"]
        .as_str()
        .expect("database")
        .to_owned();
    Ok((path, database))
}

async fn fixture(
    environment: &Environment,
    database: &str,
    name: &str,
    pg: &str,
    ch: &str,
    suffix: &str,
) -> TestResult {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE {}.{} ({pg})",
        identifier(&environment.schema),
        identifier(name)
    )))
    .execute(&environment.pg)
    .await?;
    environment
        .ch
        .query(&format!(
            "CREATE TABLE {}.{} ({ch}) {suffix}",
            identifier(database),
            identifier(name)
        ))
        .execute()
        .await?;
    Ok(())
}

async fn definition(environment: &Environment, database: &str, name: &str) -> TestResult<String> {
    Ok(environment
        .ch
        .query("SELECT create_table_query FROM system.tables WHERE database = ? AND name = ?")
        .bind(database)
        .bind(name)
        .fetch_one()
        .await?)
}

async fn success(environment: &Environment) -> TestResult {
    let (path, database) = config(environment, "ddl-success").await?;
    let name = "ddl_values";
    fixture(environment, &database, name,
        "id bigint NOT NULL, amount bigint NOT NULL, wide integer, note text, price numeric(18,4) NOT NULL, score double precision NOT NULL, day date NOT NULL, lc text, added uuid NOT NULL, location point",
        "id Int64, amount Int32, wide Int64, note String DEFAULT 'original' COMMENT 'keep this' CODEC(ZSTD(1)), price Decimal(18,2), score Float32, day Date, lc LowCardinality(String), extra UInt8",
        "ENGINE=MergeTree ORDER BY id").await?;
    let table = format!("{}.{}", identifier(&database), identifier(name));
    environment.ch.query(&format!("INSERT INTO {table} VALUES (1,-2147483648,9223372036854775807,'old',9999999999999999.99,1.25,'2149-06-06','city',7),(2,2147483647,-9223372036854775808,'other',-9999999999999999.99,-1.25,'1970-01-01','town',8)")).execute().await?;
    let values_query = format!(
        "SELECT toJSONString(tuple(id,amount,wide,note,toDecimal128(price,4),toFloat64(score),day,lc,extra)) FROM {table} ORDER BY id"
    );
    let before: Vec<String> = environment.ch.query(&values_query).fetch_all().await?;
    let original = definition(environment, &database, name).await?;
    let plan = report(
        "plan-ready",
        &environment.run_command("plan", &path, "json", &[])?,
        0,
        "ready",
    )?;
    assert_eq!(
        plan["tables"][0]["operations"]
            .as_array()
            .expect("operations")
            .len(),
        9
    );
    let sql = environment.run_command("plan", &path, "sql", &[])?;
    assert_eq!(sql.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&sql.stdout)
            .matches("ALTER TABLE")
            .count(),
        1
    );
    assert_eq!(original, definition(environment, &database, name).await?);
    let proxy = Proxy::start(&environment.ch_url, Fault::None)?;
    let output = environment.run_command("apply", &path, "json", &[("CH_URL", &proxy.url)])?;
    let queries = proxy.finish()?;
    assert_eq!(alter_count(&queries), 1);
    let applied = report("success", &output, 0, "completed")?;
    assert_eq!(applied["tables"][0]["execution"], "completed");
    let after: Vec<String> = environment.ch.query(&values_query).fetch_all().await?;
    assert_eq!(before, after, "ALTER changed existing values");
    let missing: u64 = environment
        .ch
        .query(&format!(
            "SELECT count() FROM {table} WHERE added IS NULL AND location IS NULL"
        ))
        .fetch_one()
        .await?;
    assert_eq!(missing, 2);
    let attributes: String = environment.ch.query("SELECT toJSONString(tuple(default_expression,comment,compression_codec)) FROM system.columns WHERE database = ? AND table = ? AND name = 'note'")
        .bind(&database).bind(name).fetch_one().await?;
    assert_eq!(attributes, r#"["'original'","keep this","CODEC(ZSTD(1))"]"#);
    let wide: String = environment
        .ch
        .query("SELECT type FROM system.columns WHERE database = ? AND table = ? AND name = 'wide'")
        .bind(&database)
        .bind(name)
        .fetch_one()
        .await?;
    assert_eq!(wide, "Nullable(Int64)");
    environment.ch.query(&format!("INSERT INTO {table} (id,amount,wide,note,price,score,day,lc,extra) VALUES (3,2147483648,NULL,NULL,1.2345,1.25,'1900-01-01',NULL,9)")).execute().await?;
    let proxy = Proxy::start(&environment.ch_url, Fault::None)?;
    let output = environment.run_command("apply", &path, "json", &[("CH_URL", &proxy.url)])?;
    assert_eq!(alter_count(&proxy.finish()?), 0);
    let repeated = report("repeat", &output, 0, "completed")?;
    assert_eq!(repeated["tables"][0]["execution"], "unchanged");
    assert_eq!(repeated["tables"][0]["operations"], json!([]));

    let (path, database) = config(environment, "ddl-identifiers").await?;
    let name = "ddl.\"Юникод";
    fixture(
        environment,
        &database,
        name,
        "\"v.Ю\" integer NOT NULL",
        "\"v.Ю\" Int16",
        "ENGINE=MergeTree ORDER BY tuple()",
    )
    .await?;
    report(
        "identifiers",
        &environment.run_command("apply", &path, "json", &[])?,
        0,
        "completed",
    )?;
    if environment.cases.iter().any(|case| case.postgis) {
        let (path, database) = config(environment, "ddl-postgis").await?;
        fixture(environment, &database, "ddl_postgis", "id bigint NOT NULL, shape spatial.geometry(PointZ,4326) NOT NULL, area spatial.geography(Polygon,4326)", "id Int64", "ENGINE=MergeTree ORDER BY id").await?;
        environment
            .ch
            .query(&format!(
                "INSERT INTO {}.ddl_postgis VALUES (1)",
                identifier(&database)
            ))
            .execute()
            .await?;
        report(
            "postgis",
            &environment.run_command("apply", &path, "json", &[])?,
            0,
            "completed",
        )?;
        let count: u64 = environment
            .ch
            .query(&format!(
                "SELECT count() FROM {}.ddl_postgis WHERE shape IS NULL AND area IS NULL",
                identifier(&database)
            ))
            .fetch_one()
            .await?;
        assert_eq!(count, 1);
    }
    Ok(())
}

pub(super) async fn tls_checks(
    environment: &Environment,
    pg_url: &str,
    ch_url: &str,
    ca: &Path,
) -> TestResult {
    let path = environment.config("ddl-tls", &[], Some(ca)).await?;
    let mut settings: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
    let database = settings["clickhouse"]["database"]
        .as_str()
        .expect("database");
    fixture(
        environment,
        database,
        "ddl_tls",
        "v integer NOT NULL, added text",
        "v Int32",
        "ENGINE=MergeTree ORDER BY tuple()",
    )
    .await?;
    let overrides = [("PG_URL", pg_url), ("CH_URL", ch_url)];
    report(
        "tls-plan",
        &environment.run_command("plan", &path, "json", &overrides)?,
        0,
        "ready",
    )?;
    report(
        "tls-apply",
        &environment.run_command("apply", &path, "json", &overrides)?,
        0,
        "completed",
    )?;
    settings["clickhouse"]
        .as_table_mut()
        .expect("clickhouse")
        .remove("ca_cert");
    fs::write(&path, toml::to_string(&settings)?)?;
    report(
        "tls-apply-untrusted",
        &environment.run_command("apply", &path, "json", &overrides)?,
        2,
        "blocked",
    )?;
    Ok(())
}

async fn peerdb_layout(environment: &Environment) -> TestResult {
    let (path, database) = config(environment, "peerdb-layout").await?;
    fixture(environment, &database, "ddl_orders",
        "id integer NOT NULL, created_at timestamptz NOT NULL, note text, search_index tsvector, added text NOT NULL",
        "id Int32, created_at DateTime64(6), note String, search_index String, _peerdb_version UInt64, _peerdb_is_deleted UInt8, _peerdb_synced_at DateTime64(9) DEFAULT now64(), independent Int64 DEFAULT id+1",
        "ENGINE=ReplacingMergeTree(_peerdb_version) ORDER BY id").await?;
    let name = format!("{}_ddl_orders", environment.schema);
    environment
        .ch
        .query(&format!(
            "RENAME TABLE {}.ddl_orders TO {}.{}",
            identifier(&database),
            identifier(&database),
            identifier(&name)
        ))
        .execute()
        .await?;
    let table = format!("{}.{}", identifier(&database), identifier(&name));
    environment.ch.query(&format!("INSERT INTO {table} (id,created_at,note,search_index,_peerdb_version,_peerdb_is_deleted) VALUES (1,'2024-01-01 12:00:00','old','text',1,0)")).execute().await?;
    let original_values = format!(
        "SELECT toJSONString(tuple(id,created_at,note,search_index,_peerdb_synced_at,independent)) FROM {table}"
    );
    let before: String = environment.ch.query(&original_values).fetch_one().await?;
    for extra in ["_peerdb_raw_example", "orders_resync"] {
        environment
            .ch
            .query(&format!(
                "CREATE TABLE {}.{} (v Int32) ENGINE=MergeTree ORDER BY tuple()",
                identifier(&database),
                identifier(extra)
            ))
            .execute()
            .await?;
    }
    let proxy = Proxy::start(&environment.ch_url, Fault::None)?;
    report(
        "unexcluded-services",
        &environment.run_command("apply", &path, "json", &[("CH_URL", &proxy.url)])?,
        1,
        "blocked",
    )?;
    assert_eq!(alter_count(&proxy.finish()?), 0);
    let mut settings: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
    settings.as_table_mut().unwrap().insert(
        "discovery".into(),
        toml::Value::try_from(
            json!({"exclude_table_prefixes":["_peerdb_raw_"], "exclude_tables":["orders_resync"]}),
        )?,
    );
    fs::write(&path, toml::to_string(&settings)?)?;
    let diff = environment.report(
        "peerdb-layout-diff",
        &environment.run(&path, "json", &[])?,
        "mismatch",
    )?;
    assert_eq!(diff["tables"].as_array().unwrap().len(), 1);
    assert_eq!(diff["skipped_tables"].as_array().unwrap().len(), 2);
    assert_eq!(diff["tables"][0]["source"]["table"], "ddl_orders");
    let plan = report(
        "peerdb-layout-plan",
        &environment.run_command("plan", &path, "json", &[])?,
        0,
        "ready",
    )?;
    assert_eq!(plan["tables"][0]["operations"].as_array().unwrap().len(), 3);
    assert_eq!(plan["skipped_tables"].as_array().unwrap().len(), 2);
    let proxy = Proxy::start(&environment.ch_url, Fault::None)?;
    let applied = report(
        "peerdb-layout-apply",
        &environment.run_command("apply", &path, "json", &[("CH_URL", &proxy.url)])?,
        0,
        "completed",
    )?;
    assert_eq!(alter_count(&proxy.finish()?), 1);
    assert_eq!(applied["skipped_tables"].as_array().unwrap().len(), 2);
    let after: String = environment.ch.query(&original_values).fetch_one().await?;
    assert_eq!(before, after);
    let added: u64 = environment
        .ch
        .query(&format!("SELECT count() FROM {table} WHERE added IS NULL"))
        .fetch_one()
        .await?;
    assert_eq!(added, 1);
    for extra in ["_peerdb_raw_example", "orders_resync"] {
        assert!(
            !definition(environment, &database, extra)
                .await?
                .contains("added")
        );
    }
    settings["discovery"]["exclude_table_prefixes"] =
        toml::Value::try_from(vec!["_peerdb_raw_", environment.schema.as_str()])?;
    fs::write(&path, toml::to_string(&settings)?)?;
    let empty = report(
        "all-excluded",
        &environment.run_command("apply", &path, "json", &[])?,
        2,
        "blocked",
    )?;
    assert_eq!(empty["errors"][0]["code"], "no_tables_selected");
    environment.report(
        "all-excluded-diff",
        &environment.run(&path, "json", &[])?,
        "unknown",
    )?;
    Ok(())
}

async fn connection_timezone(environment: &Environment) -> TestResult {
    let (path, database) = config(environment, "connection-timezone").await?;
    fixture(
        environment,
        &database,
        "ddl_timezone_context",
        "id integer NOT NULL, v timestamptz",
        "id Int32, v DateTime64(6)",
        "ENGINE=MergeTree ORDER BY id",
    )
    .await?;
    let user = format!("{}_timezone", environment.database);
    environment.ch.query(&format!("CREATE USER {} IDENTIFIED WITH sha256_password BY 'timezone-pass' SETTINGS session_timezone = 'Europe/Berlin'", identifier(&user))).execute().await?;
    environment
        .ch
        .query(&format!(
            "GRANT SHOW TABLES, SHOW COLUMNS ON *.* TO {}",
            identifier(&user)
        ))
        .execute()
        .await?;
    for table in [
        "data_skipping_indices",
        "projections",
        "mutations",
        "processes",
    ] {
        environment
            .ch
            .query(&format!(
                "GRANT SELECT ON system.{table} TO {}",
                identifier(&user)
            ))
            .execute()
            .await?;
    }
    environment
        .ch
        .query(&format!(
            "GRANT ALTER MODIFY COLUMN ON {}.* TO {}",
            identifier(&database),
            identifier(&user)
        ))
        .execute()
        .await?;
    let overrides = [("CH_USER", user.as_str()), ("CH_PASSWORD", "timezone-pass")];
    let diff = environment.report(
        "timezone-berlin",
        &environment.run(&path, "json", &overrides)?,
        "mismatch",
    )?;
    assert_eq!(diff["tables"][0]["default_timezone"], "Europe/Berlin");
    assert!(
        diff["tables"][0]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "timezone_mismatch")
    );
    report(
        "timezone-berlin-apply",
        &environment.run_command("apply", &path, "json", &overrides)?,
        1,
        "blocked",
    )?;
    environment
        .ch
        .query(&format!(
            "ALTER USER {} SETTINGS session_timezone = 'UTC'",
            identifier(&user)
        ))
        .execute()
        .await?;
    report(
        "timezone-utc-apply",
        &environment.run_command("apply", &path, "json", &overrides)?,
        0,
        "completed",
    )?;
    environment.report(
        "timezone-utc-diff",
        &environment.run(&path, "json", &overrides)?,
        "compatible",
    )?;
    environment
        .ch
        .query(&format!("DROP USER {}", identifier(&user)))
        .execute()
        .await?;
    Ok(())
}

async fn blockers(environment: &Environment) -> TestResult {
    for (case, pg, ch, suffix, code, exit) in [
        (
            "key",
            "v bigint NOT NULL",
            "v Int32",
            "ENGINE=MergeTree ORDER BY v",
            "key_column_change",
            1,
        ),
        (
            "expression",
            "v bigint NOT NULL",
            "v Int32, derived Int64 DEFAULT v+1",
            "ENGINE=MergeTree ORDER BY tuple()",
            "dependent_column_expression",
            1,
        ),
        (
            "ttl",
            "v bigint NOT NULL",
            "v Int32, ts DateTime",
            "ENGINE=MergeTree ORDER BY tuple() TTL ts + INTERVAL 1 DAY",
            "dependent_table_definition",
            1,
        ),
        (
            "constraint",
            "v bigint NOT NULL",
            "v Int32, CONSTRAINT c CHECK v > 0",
            "ENGINE=MergeTree ORDER BY tuple()",
            "dependent_table_definition",
            1,
        ),
        (
            "index",
            "v bigint NOT NULL",
            "v Int32, INDEX i v TYPE minmax GRANULARITY 1",
            "ENGINE=MergeTree ORDER BY tuple()",
            "dependent_index_or_projection",
            1,
        ),
        (
            "projection",
            "v bigint NOT NULL",
            "v Int32, PROJECTION p (SELECT v ORDER BY v)",
            "ENGINE=MergeTree ORDER BY tuple()",
            "dependent_index_or_projection",
            1,
        ),
        (
            "engine",
            "v bigint NOT NULL",
            "v UInt32",
            "ENGINE=ReplacingMergeTree(v) ORDER BY tuple()",
            "unsupported_alter_conversion",
            1,
        ),
        (
            "engine_argument",
            "v date NOT NULL",
            "v Date",
            "ENGINE=ReplacingMergeTree(v) ORDER BY tuple()",
            "engine_column_change",
            1,
        ),
        (
            "unknown",
            "v integer[] NOT NULL",
            "v Array(Int32)",
            "ENGINE=MergeTree ORDER BY tuple()",
            "unsupported_source_type",
            2,
        ),
        (
            "materialized",
            "v integer NOT NULL",
            "id Int32, v Int32 MATERIALIZED id",
            "ENGINE=MergeTree ORDER BY id",
            "column_not_writable",
            1,
        ),
        (
            "timezone",
            "v timestamptz NOT NULL",
            "v DateTime64(6,'Europe/Berlin')",
            "ENGINE=MergeTree ORDER BY tuple()",
            "unsupported_alter_conversion",
            1,
        ),
    ] {
        let (path, database) = config(environment, &format!("ddl-block-{case}")).await?;
        let allowed = format!("a_ddl_{case}");
        let blocked = format!("z_ddl_{case}");
        fixture(
            environment,
            &database,
            &allowed,
            "v integer NOT NULL, added text",
            "v Int32",
            "ENGINE=MergeTree ORDER BY tuple()",
        )
        .await?;
        fixture(environment, &database, &blocked, pg, ch, suffix).await?;
        let before = [
            definition(environment, &database, &allowed).await?,
            definition(environment, &database, &blocked).await?,
        ];
        let proxy = Proxy::start(&environment.ch_url, Fault::None)?;
        let output = environment.run_command("apply", &path, "json", &[("CH_URL", &proxy.url)])?;
        assert_eq!(
            alter_count(&proxy.finish()?),
            0,
            "{case}: sent ALTER despite blocker"
        );
        let plan = report(case, &output, exit, "blocked")?;
        assert_eq!(plan["tables"][0]["execution"], "not_sent");
        let mut found = false;
        for blocker in plan["tables"][1]["blockers"].as_array().expect("blockers") {
            if blocker["code"] == code {
                found = true;
            }
        }
        assert!(found, "{case}: missing {code}: {plan}");
        assert_eq!(
            before,
            [
                definition(environment, &database, &allowed).await?,
                definition(environment, &database, &blocked).await?
            ]
        );
        let output = environment.run_command("plan", &path, "sql", &[])?;
        assert_eq!(output.status.code(), Some(exit));
        assert!(
            output.stdout.is_empty(),
            "blocked SQL plan emitted executable statements"
        );
        assert!(!output.stderr.is_empty());
    }
    Ok(())
}

async fn permissions(environment: &Environment) -> TestResult {
    let (path, database) = config(environment, "ddl-rights").await?;
    for name in ["a_ddl_rights", "z_ddl_rights"] {
        fixture(
            environment,
            &database,
            name,
            "v integer NOT NULL, added text",
            "v Int32",
            "ENGINE=MergeTree ORDER BY tuple()",
        )
        .await?;
    }
    let role = format!("{}_ddl_reader", environment.database);
    environment
        .ch
        .query(&format!(
            "CREATE USER {} IDENTIFIED WITH sha256_password BY 'ddl-test-password'",
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
    environment
        .ch
        .query(&format!(
            "GRANT ALTER ADD COLUMN ON {}.a_ddl_rights TO {}",
            identifier(&database),
            identifier(&role)
        ))
        .execute()
        .await?;
    report(
        "metadata-permissions",
        &environment.run_command(
            "plan",
            &path,
            "json",
            &[("CH_USER", &role), ("CH_PASSWORD", "ddl-test-password")],
        )?,
        2,
        "blocked",
    )?;
    for table in [
        "data_skipping_indices",
        "projections",
        "mutations",
        "processes",
    ] {
        environment
            .ch
            .query(&format!(
                "GRANT SELECT ON system.{table} TO {}",
                identifier(&role)
            ))
            .execute()
            .await?;
    }
    let plan = environment.run_command(
        "plan",
        &path,
        "json",
        &[("CH_USER", &role), ("CH_PASSWORD", "ddl-test-password")],
    )?;
    report("readonly-plan", &plan, 0, "ready")?;
    let proxy = Proxy::start(&environment.ch_url, Fault::None)?;
    let output = environment.run_command(
        "apply",
        &path,
        "json",
        &[
            ("CH_URL", &proxy.url),
            ("CH_USER", &role),
            ("CH_PASSWORD", "ddl-test-password"),
        ],
    )?;
    assert_eq!(alter_count(&proxy.finish()?), 0);
    let blocked = report("permissions", &output, 1, "blocked")?;
    assert_eq!(
        blocked["tables"][1]["blockers"][0]["code"],
        "ddl_permission_denied"
    );
    environment
        .ch
        .query(&format!(
            "GRANT ALTER ADD COLUMN ON {}.z_ddl_rights TO {}",
            identifier(&database),
            identifier(&role)
        ))
        .execute()
        .await?;
    report(
        "least-privilege-apply",
        &environment.run_command(
            "apply",
            &path,
            "json",
            &[("CH_USER", &role), ("CH_PASSWORD", "ddl-test-password")],
        )?,
        0,
        "completed",
    )?;
    environment
        .ch
        .query(&format!("DROP USER {}", identifier(&role)))
        .execute()
        .await?;
    Ok(())
}

async fn races_and_dependencies(environment: &Environment) -> TestResult {
    for (label, fault, expected) in [
        ("race", Fault::ChangeBeforeApply, "blocked"),
        ("verification", Fault::RejectVerification, "indeterminate"),
        ("view", Fault::None, "blocked"),
        ("ambiguous", Fault::None, "blocked"),
    ] {
        let (path, database) = config(environment, &format!("ddl-{label}")).await?;
        for prefix in ["a", "z"] {
            fixture(
                environment,
                &database,
                &format!("{prefix}_ddl_{label}"),
                "v integer NOT NULL, added text",
                "v Int32",
                "ENGINE=MergeTree ORDER BY tuple()",
            )
            .await?;
        }
        if label == "view" {
            environment.ch.query(&format!("CREATE MATERIALIZED VIEW {}.ddl_external_view ENGINE=MergeTree ORDER BY tuple() AS SELECT v FROM {}.z_ddl_view", identifier(&environment.database), identifier(&database))).execute().await?;
        }
        if label == "ambiguous" {
            let target = format!("{}_z_ddl_ambiguous", environment.schema);
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TABLE {}.{} (v integer NOT NULL, added text)",
                identifier(&environment.schema),
                identifier(&target)
            )))
            .execute(&environment.pg)
            .await?;
            environment
                .ch
                .query(&format!(
                    "RENAME TABLE {}.z_ddl_ambiguous TO {}.{}",
                    identifier(&database),
                    identifier(&database),
                    identifier(&target)
                ))
                .execute()
                .await?;
        }
        let proxy = Proxy::start(&environment.ch_url, fault)?;
        let output = environment.run_command("apply", &path, "json", &[("CH_URL", &proxy.url)])?;
        let statements = proxy.finish()?;
        assert_eq!(
            alter_count(&statements),
            if label == "verification" { 1 } else { 0 },
            "{label}"
        );
        let value = report(
            label,
            &output,
            if label == "view" { 1 } else { 2 },
            expected,
        )?;
        if label == "verification" {
            assert_eq!(value["tables"][0]["execution"], "indeterminate");
            assert_eq!(value["tables"][1]["execution"], "not_sent");
        } else {
            let expected_code = match label {
                "race" => "schema_changed_during_check",
                "view" => "dependent_views",
                _ => "ambiguous_source_table",
            };
            assert!(
                value["tables"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|table| table["blockers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|blocker| blocker["code"] == expected_code)),
                "{label}: {value}"
            );
        }
        if label == "view" {
            environment
                .ch
                .query(&format!(
                    "DROP TABLE {}.ddl_external_view",
                    identifier(&environment.database)
                ))
                .execute()
                .await?;
        }
    }
    Ok(())
}

async fn runtime_failures(environment: &Environment) -> TestResult {
    for (label, fault, status, execution) in [
        ("rejected", Fault::RejectSecond, "partial", "failed"),
        (
            "timeout",
            Fault::TimeoutSecond,
            "indeterminate",
            "indeterminate",
        ),
    ] {
        let (path, database) = config(environment, &format!("ddl-runtime-{label}")).await?;
        let mut settings: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
        settings.as_table_mut().expect("config table").insert(
            "apply".into(),
            toml::Value::try_from(json!({"ddl_timeout_seconds":1}))?,
        );
        fs::write(&path, toml::to_string(&settings)?)?;
        for prefix in ["a", "b", "c"] {
            fixture(
                environment,
                &database,
                &format!("{prefix}_ddl_{label}"),
                "v integer NOT NULL, added text",
                "v Int32",
                "ENGINE=MergeTree ORDER BY tuple()",
            )
            .await?;
        }
        let proxy = Proxy::start(&environment.ch_url, fault)?;
        let output = environment.run_command("apply", &path, "json", &[("CH_URL", &proxy.url)])?;
        assert_eq!(
            alter_count(&proxy.finish()?),
            2,
            "retried DDL or sent a later command"
        );
        let applied = report(label, &output, 2, status)?;
        assert_eq!(applied["tables"][0]["execution"], "completed");
        assert_eq!(applied["tables"][1]["execution"], execution);
        assert_eq!(applied["tables"][2]["execution"], "not_sent");
        assert!(!applied["tables"][1]["query_id"].is_null());
        let third = definition(environment, &database, &format!("c_ddl_{label}")).await?;
        assert!(!third.contains("added"));
        let second = definition(environment, &database, &format!("b_ddl_{label}")).await?;
        assert_eq!(
            second.contains("added"),
            label == "timeout",
            "lost response is not a rollback"
        );
    }
    Ok(())
}

fn alter_count(queries: &[String]) -> usize {
    queries
        .iter()
        .filter(|sql| sql.starts_with("ALTER TABLE "))
        .count()
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    RejectSecond,
    TimeoutSecond,
    ChangeBeforeApply,
    RejectVerification,
}

struct Proxy {
    url: String,
    stop: Arc<AtomicBool>,
    server: thread::JoinHandle<TestResult<Vec<String>>>,
}

impl Proxy {
    fn start(endpoint: &str, fault: Fault) -> TestResult<Self> {
        let endpoint = Url::parse(endpoint)?;
        assert_eq!(endpoint.scheme(), "http");
        let address = format!(
            "{}:{}",
            endpoint.host_str().ok_or("missing host")?,
            endpoint.port_or_known_default().ok_or("missing port")?
        );
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}", listener.local_addr()?);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let server = thread::spawn(move || -> TestResult<Vec<String>> {
            let queries = Arc::new(Mutex::new(Vec::new()));
            let alters = Arc::new(AtomicUsize::new(0));
            let mut handlers = Vec::new();
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let address = address.clone();
                        let queries = queries.clone();
                        let alters = alters.clone();
                        handlers.push(thread::spawn(move || {
                            proxy_request(stream, &address, fault, &queries, &alters)
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if stopped.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            for handler in handlers {
                handler.join().map_err(|_| "proxy handler panicked")??;
            }
            let queries = queries.lock().map_err(|_| "proxy log poisoned")?.clone();
            Ok(queries)
        });
        Ok(Self { url, stop, server })
    }
    fn finish(self) -> TestResult<Vec<String>> {
        self.stop.store(true, Ordering::SeqCst);
        self.server.join().map_err(|_| "proxy panicked")?
    }
}

fn proxy_request(
    mut stream: TcpStream,
    address: &str,
    fault: Fault,
    queries: &Mutex<Vec<String>>,
    alters: &AtomicUsize,
) -> TestResult {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let mut length = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" {
            break;
        }
        if line.is_empty() {
            return Err("incomplete proxy request".into());
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>()?);
            }
            if name.eq_ignore_ascii_case("connection") {
                continue;
            }
        }
        request.push_str(&line);
    }
    request.push_str("Connection: close\r\n\r\n");
    let length = length.ok_or("proxy requires Content-Length")?;
    assert!(length < 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let sql = String::from_utf8(body.clone())?;
    let alter = if sql.starts_with("ALTER TABLE ") {
        alters.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        0
    };
    queries
        .lock()
        .map_err(|_| "proxy log poisoned")?
        .push(sql.clone());
    if matches!(fault, Fault::ChangeBeforeApply) && sql.starts_with("CHECK GRANT ALTER ") {
        let table = sql
            .split_once(" ON ")
            .ok_or("grant target missing")?
            .1
            .split(" FORMAT ")
            .next()
            .ok_or("grant format missing")?;
        let change = format!("ALTER TABLE {table} COMMENT COLUMN v 'concurrent DDL'");
        let headers = request
            .lines()
            .filter(|line| {
                !line.to_ascii_lowercase().starts_with("content-length:") && !line.is_empty()
            })
            .collect::<Vec<_>>()
            .join("\r\n");
        let headers = format!("{headers}\r\nContent-Length: {}\r\n\r\n", change.len());
        let changed = forward(address, &headers, change.as_bytes())?;
        assert!(
            changed.starts_with(b"HTTP/1.1 200"),
            "race injection failed: {}",
            String::from_utf8_lossy(&changed)
        );
    }
    if matches!(fault, Fault::RejectVerification) && alter == 0 && alters.load(Ordering::SeqCst) > 0
    {
        let message = "Code: 497. DB::Exception: synthetic metadata failure";
        write!(
            stream,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
            message.len()
        )?;
        return Ok(());
    }
    if alter == 2 && matches!(fault, Fault::RejectSecond) {
        let message = "Code: 497. DB::Exception: synthetic permission failure";
        write!(
            stream,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
            message.len()
        )?;
        return Ok(());
    }
    let response = forward(address, &request, &body)?;
    if alter == 2 && matches!(fault, Fault::TimeoutSecond) {
        assert!(response.starts_with(b"HTTP/1.1 200"));
        thread::sleep(Duration::from_millis(1500));
        return Ok(());
    }
    stream.write_all(&response)?;
    Ok(())
}

fn forward(address: &str, headers: &str, body: &[u8]) -> TestResult<Vec<u8>> {
    let mut upstream = TcpStream::connect(address)?;
    upstream.set_read_timeout(Some(Duration::from_secs(15)))?;
    upstream.write_all(headers.as_bytes())?;
    upstream.write_all(body)?;
    let mut response = Vec::new();
    upstream.read_to_end(&mut response)?;
    Ok(response)
}
