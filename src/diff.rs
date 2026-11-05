//! Pure, deterministic comparison of catalog snapshots.

use std::collections::{BTreeMap, BTreeSet};

use crate::compatibility::{compare_types, expected_definition};
use crate::config::TablePair;
use crate::model::{ChColumn, ChTable, Finding, PgColumn, PgTable, Status, TableDiff};

/// Compares one configured pair without performing I/O or modifying schemas.
///
/// `None` denotes a confirmed missing table. Catalog read errors must be reported
/// by the caller rather than converted to `None`.
#[must_use]
pub fn compare(pair: &TablePair, source: Option<&PgTable>, target: Option<&ChTable>) -> TableDiff {
    let mut result = TableDiff {
        source: source.map(|_| pair.source.clone()),
        source_candidates: match source {
            Some(_) => vec![pair.source.clone()],
            None => Vec::new(),
        },
        target: pair.target.clone(),
        status: Status::Match,
        findings: Vec::new(),
        ignored_target_columns: pair.ignore_target_columns.clone(),
    };
    let mut ignored = BTreeSet::new();
    for name in &pair.ignore_target_columns {
        ignored.insert(name.as_str());
    }
    match source {
        Some(source) => {
            if (source.kind != "r" && source.kind != "p") || source.persistence != "p" {
                result.findings.push(table_finding(
                    "unsupported_source_object",
                    Status::Unknown,
                    "only permanent ordinary tables and partitioned parents are supported",
                ));
            }
            for column in &source.columns {
                if ignored.contains(column.name.as_str()) {
                    result.findings.push(column_finding(
                        "ignore_source_column_conflict",
                        Status::Unknown,
                        Some(column),
                        None,
                        None,
                        "an ignore entry cannot hide a column that exists in PostgreSQL".into(),
                    ));
                }
            }
        }
        None => result.findings.push(table_finding(
            "source_table_missing",
            Status::Mismatch,
            "configured source table does not exist",
        )),
    }
    match target {
        Some(target) => {
            if !supported_engine(&target.engine) {
                result.findings.push(table_finding(
                    "unsupported_target_object",
                    Status::Unknown,
                    "only recognized MergeTree-family engines are supported",
                ));
            }
        }
        None => result.findings.push(table_finding(
            "target_table_missing",
            Status::Mismatch,
            "configured target table does not exist",
        )),
    }
    let (Some(source), Some(target)) = (source, target) else {
        finalize(&mut result);
        return result;
    };
    let source_columns = source_columns(source, &mut result.findings);
    let target_columns = target_columns(target, &mut result.findings);
    for (name, source) in &source_columns {
        let Some(target) = target_columns.get(name) else {
            let expected = match expected_definition(&source.data_type) {
                Ok(expected) if source.nullable => Some(format!("Nullable({expected})")),
                Ok(expected) => Some(expected),
                Err(message) => {
                    result.findings.push(column_finding(
                        "unsupported_source_type",
                        Status::Unknown,
                        Some(source),
                        None,
                        None,
                        message,
                    ));
                    None
                }
            };
            result.findings.push(column_finding(
                "source_column_missing_in_target",
                Status::Mismatch,
                Some(source),
                None,
                expected,
                "source column is absent from the configured target table".into(),
            ));
            if source.generated {
                result.findings.push(generated_finding(source, None));
            }
            continue;
        };
        compare_column(source, target, &mut result.findings);
    }
    for (name, target) in &target_columns {
        if source_columns.contains_key(name) {
            continue;
        }
        if ignored.contains(name) {
            continue;
        }
        result.findings.push(column_finding(
            "target_only_column", Status::Mismatch, None, Some(target), None,
            "target-only column is outside the agreed column set; add an explicit ignore entry if intended".into(),
        ));
    }
    finalize(&mut result);
    result
}

fn source_columns<'a>(
    table: &'a PgTable,
    findings: &mut Vec<Finding>,
) -> BTreeMap<&'a str, &'a PgColumn> {
    let mut columns = BTreeMap::new();
    for column in &table.columns {
        if columns.insert(column.name.as_str(), column).is_some() {
            findings.push(column_finding(
                "duplicate_source_column",
                Status::Unknown,
                Some(column),
                None,
                None,
                "source metadata contains a duplicate column identifier".into(),
            ));
        }
    }
    columns
}

fn target_columns<'a>(
    table: &'a ChTable,
    findings: &mut Vec<Finding>,
) -> BTreeMap<&'a str, &'a ChColumn> {
    let mut columns = BTreeMap::new();
    for column in &table.columns {
        if columns.insert(column.name.as_str(), column).is_some() {
            findings.push(column_finding(
                "duplicate_target_column",
                Status::Unknown,
                None,
                Some(column),
                None,
                "target metadata contains a duplicate column identifier".into(),
            ));
        }
    }
    columns
}

fn compare_column(source: &PgColumn, target: &ChColumn, findings: &mut Vec<Finding>) {
    if source.generated {
        findings.push(generated_finding(source, Some(target)));
    }
    match target.default_kind.as_str() {
        "" | "DEFAULT" => {}
        "ALIAS" | "MATERIALIZED" => findings.push(column_finding(
            "column_not_writable",
            Status::Mismatch,
            Some(source),
            Some(target),
            None,
            "ALIAS/MATERIALIZED target columns are not ordinary writable destinations".into(),
        )),
        kind => findings.push(column_finding(
            "unsupported_column_kind",
            Status::Unknown,
            Some(source),
            Some(target),
            None,
            format!("target default kind {kind:?} has no supported v1 write rule"),
        )),
    }
    let comparison = compare_types(&source.data_type, &target.data_type);
    let expected = match comparison.expected {
        Some(expected) if source.nullable => Some(format!("Nullable({expected})")),
        Some(expected) => Some(expected),
        None => None,
    };
    findings.push(column_finding(
        comparison.code,
        comparison.status,
        Some(source),
        Some(target),
        expected.clone(),
        comparison.message,
    ));
    match comparison.target_nullable {
        Some(true) if !source.nullable => findings.push(column_finding(
            "nullable_widening",
            Status::Compatible,
            Some(source),
            Some(target),
            expected.clone(),
            "the target permits NULL in addition to all source values".into(),
        )),
        Some(false) if source.nullable => findings.push(column_finding(
            "nullability_mismatch",
            Status::Mismatch,
            Some(source),
            Some(target),
            expected.clone(),
            "the source permits NULL but the target type does not; DEFAULT does not compensate"
                .into(),
        )),
        Some(true) | Some(false) | None => {}
    }
    if comparison.geo {
        findings.push(column_finding(
            "not_represented_in_target_schema", Status::Match, Some(source), Some(target), expected,
            "the string geometry policy does not represent source subtype, SRID, or dimensions in the target schema; declared source metadata is retained, arbitrary CHECK constraints and serialized values are not validated".into(),
        ));
    }
}

fn generated_finding(source: &PgColumn, target: Option<&ChColumn>) -> Finding {
    column_finding(
        "unsupported_generated_column",
        Status::Unknown,
        Some(source),
        target,
        None,
        "delivery of PostgreSQL generated columns is outside the v1 policy".into(),
    )
}

fn supported_engine(engine: &str) -> bool {
    let engine = engine
        .strip_prefix("Replicated")
        .or_else(|| engine.strip_prefix("Shared"))
        .unwrap_or(engine);
    const ENGINES: [&str; 7] = [
        "MergeTree",
        "ReplacingMergeTree",
        "SummingMergeTree",
        "AggregatingMergeTree",
        "CollapsingMergeTree",
        "VersionedCollapsingMergeTree",
        "GraphiteMergeTree",
    ];
    ENGINES.contains(&engine)
}

fn table_finding(code: &str, status: Status, message: &str) -> Finding {
    Finding {
        column: None,
        code: code.into(),
        status,
        source: None,
        expected: None,
        target: None,
        message: message.into(),
    }
}

fn column_finding(
    code: &str,
    status: Status,
    source: Option<&PgColumn>,
    target: Option<&ChColumn>,
    expected: Option<String>,
    message: String,
) -> Finding {
    let column = match (source, target) {
        (Some(source), Some(_) | None) => Some(source.name.clone()),
        (None, Some(target)) => Some(target.name.clone()),
        (None, None) => None,
    };
    Finding {
        column,
        code: code.into(),
        status,
        source: source.cloned(),
        expected,
        target: target.cloned(),
        message,
    }
}

fn finalize(result: &mut TableDiff) {
    result
        .findings
        .sort_by(|left, right| (&left.column, &left.code).cmp(&(&right.column, &right.code)));
    for finding in &result.findings {
        result.status = result.status.max(finding.status);
    }
    result.ignored_target_columns.sort();
    result.ignored_target_columns.dedup();
}

#[cfg(test)]
mod tests {
    use crate::config::{SourceTable, TablePair, TargetTable};
    use crate::model::{ChColumn, ChTable, GeoMetadata, PgColumn, PgTable, PgType, Status};

    use super::compare;

    fn pair() -> TablePair {
        TablePair {
            source: SourceTable {
                schema: "public".into(),
                table: "events".into(),
            },
            target: TargetTable {
                database: "analytics".into(),
                table: "events".into(),
            },
            ignore_target_columns: Vec::new(),
        }
    }

    fn source_column(name: &str, nullable: bool) -> PgColumn {
        PgColumn {
            name: name.into(),
            data_type: PgType {
                schema: "pg_catalog".into(),
                name: "int4".into(),
                definition: "integer".into(),
                kind: "b".into(),
                typmod: -1,
                extension: None,
                geo: None,
            },
            nullable,
            generated: false,
        }
    }

    fn target_column(name: &str, data_type: &str) -> ChColumn {
        ChColumn {
            name: name.into(),
            data_type: data_type.into(),
            default_kind: String::new(),
        }
    }

    fn source(columns: Vec<PgColumn>) -> PgTable {
        PgTable {
            kind: "r".into(),
            persistence: "p".into(),
            columns,
        }
    }

    fn target(columns: Vec<ChColumn>) -> ChTable {
        ChTable {
            engine: "MergeTree".into(),
            columns,
        }
    }

    #[test]
    fn order_is_ignored_and_identifiers_are_exact() {
        let source = source(vec![
            source_column("b", false),
            source_column("a.with.точка", false),
        ]);
        let target = target(vec![
            target_column("a.with.точка", "Int32"),
            target_column("b", "Int32"),
        ]);
        let result = compare(&pair(), Some(&source), Some(&target));
        assert_eq!(result.status, Status::Match);
        assert_eq!(result.findings.len(), 2);
        for finding in &result.findings {
            assert_eq!(finding.code, "type_match");
        }
        let target = super::tests::target(vec![
            target_column("B", "Int32"),
            target_column("a.with.точка", "Int32"),
        ]);
        let result = compare(&pair(), Some(&source), Some(&target));
        assert_eq!(result.status, Status::Mismatch);
        assert_eq!(result.findings.len(), 3);
        assert_eq!(result.findings[0].column.as_deref(), Some("B"));
        assert_eq!(result.findings[2].column.as_deref(), Some("b"));
    }

    #[test]
    fn nullability_is_directional_and_defaults_do_not_hide_mismatch() {
        for (nullable, target_type, status, codes) in [
            (false, "Int32", Status::Match, vec!["type_match"]),
            (
                false,
                "Nullable(Int32)",
                Status::Compatible,
                vec!["nullable_widening", "type_match"],
            ),
            (true, "Nullable(Int32)", Status::Match, vec!["type_match"]),
            (
                true,
                "Int64",
                Status::Mismatch,
                vec!["nullability_mismatch", "type_widening"],
            ),
        ] {
            let source = source(vec![source_column("id", nullable)]);
            let mut column = target_column("id", target_type);
            column.default_kind = "DEFAULT".into();
            let target = target(vec![column]);
            let result = compare(&pair(), Some(&source), Some(&target));
            assert_eq!(result.status, status);
            let mut actual = Vec::new();
            for finding in &result.findings {
                actual.push(finding.code.as_str());
            }
            assert_eq!(actual, codes);
        }
    }

    #[test]
    fn only_explicit_target_only_ignores_are_applied() {
        let source = source(vec![source_column("id", false)]);
        let target = target(vec![
            target_column("id", "Int32"),
            target_column("_peerdb_version", "UInt64"),
        ]);
        let mut pair = pair();
        assert_eq!(
            compare(&pair, Some(&source), Some(&target)).status,
            Status::Mismatch
        );
        pair.ignore_target_columns = vec!["_peerdb_version".into(), "not_present".into()];
        let result = compare(&pair, Some(&source), Some(&target));
        assert_eq!(result.status, Status::Match);
        assert_eq!(
            result.ignored_target_columns,
            ["_peerdb_version", "not_present"]
        );
        pair.ignore_target_columns.push("id".into());
        let result = compare(&pair, Some(&source), Some(&target));
        assert_eq!(result.status, Status::Unknown);
        assert_eq!(result.findings[0].code, "ignore_source_column_conflict");
    }

    #[test]
    fn target_only_default_materialized_and_nullable_columns_still_drift() {
        let source = source(vec![source_column("id", false)]);
        let mut columns = vec![target_column("id", "Int32")];
        for (name, kind, data_type) in [
            ("a", "DEFAULT", "String"),
            ("b", "MATERIALIZED", "Int32"),
            ("c", "", "Nullable(String)"),
        ] {
            let mut column = target_column(name, data_type);
            column.default_kind = kind.into();
            columns.push(column);
        }
        let result = compare(&pair(), Some(&source), Some(&target(columns)));
        assert_eq!(result.status, Status::Mismatch);
        assert_eq!(result.findings.len(), 4);
        for finding in &result.findings[..3] {
            assert_eq!(finding.code, "target_only_column");
        }
    }

    #[test]
    fn write_restrictions_and_generated_columns_are_reported_independently() {
        for (kind, status, code) in [
            ("ALIAS", Status::Mismatch, "column_not_writable"),
            ("MATERIALIZED", Status::Mismatch, "column_not_writable"),
            ("EPHEMERAL", Status::Unknown, "unsupported_column_kind"),
        ] {
            let source = source(vec![source_column("id", false)]);
            let mut column = target_column("id", "Int32");
            column.default_kind = kind.into();
            let result = compare(&pair(), Some(&source), Some(&target(vec![column])));
            assert_eq!(result.status, status);
            assert_eq!(result.findings.len(), 2);
            let mut found = false;
            for finding in result.findings {
                if finding.code == code {
                    assert_eq!(finding.status, status);
                    found = true;
                }
            }
            assert!(found);
        }
        let mut column = source_column("id", false);
        column.generated = true;
        let result = compare(
            &pair(),
            Some(&source(vec![column])),
            Some(&target(vec![target_column("id", "Int16")])),
        );
        assert_eq!(result.status, Status::Unknown);
        assert_eq!(result.findings.len(), 2);
    }

    #[test]
    fn unsupported_objects_and_duplicate_metadata_never_pass() {
        let mut source = source(vec![source_column("id", false)]);
        let mut target = target(vec![target_column("id", "Int32")]);
        source.kind = "v".into();
        target.engine = "Distributed".into();
        let result = compare(&pair(), Some(&source), Some(&target));
        assert_eq!(result.status, Status::Unknown);
        assert_eq!(result.findings.len(), 3);
        source.kind = "p".into();
        target.engine = "ReplicatedReplacingMergeTree".into();
        assert_eq!(
            compare(&pair(), Some(&source), Some(&target)).status,
            Status::Match
        );
        source.persistence = "u".into();
        assert_eq!(
            compare(&pair(), Some(&source), Some(&target)).status,
            Status::Unknown
        );
        source.persistence = "p".into();
        source.columns.push(source_column("id", false));
        target.columns.push(target_column("id", "Int32"));
        let result = compare(&pair(), Some(&source), Some(&target));
        assert_eq!(result.status, Status::Unknown);
        assert_eq!(result.findings.len(), 3);
    }

    #[test]
    fn missing_tables_are_structural_findings_not_empty_successes() {
        let result = compare(&pair(), None, None);
        assert_eq!(result.status, Status::Mismatch);
        assert_eq!(result.findings.len(), 2);
        assert_eq!(result.findings[0].code, "source_table_missing");
        assert_eq!(result.findings[1].code, "target_table_missing");
    }

    #[test]
    fn missing_columns_keep_expected_types_and_unsupported_source_status() {
        let mut unsupported = source_column("z_array", false);
        unsupported.data_type.name = "_int4".into();
        let source = source(vec![source_column("a_id", true), unsupported]);
        let result = compare(&pair(), Some(&source), Some(&target(Vec::new())));
        assert_eq!(result.status, Status::Unknown);
        assert_eq!(result.findings.len(), 3);
        assert_eq!(
            result.findings[0].expected.as_deref(),
            Some("Nullable(Int32)")
        );
        assert_eq!(result.findings[1].code, "source_column_missing_in_target");
        assert_eq!(result.findings[2].code, "unsupported_source_type");
    }

    #[test]
    fn unknown_preserves_other_mismatches_and_compatible_findings() {
        let mut unknown = source_column("unknown", false);
        unknown.data_type.name = "_int4".into();
        let source = source(vec![
            source_column("widened", false),
            source_column("missing", false),
            unknown,
        ]);
        let target = target(vec![
            target_column("widened", "Int64"),
            target_column("unknown", "Array(Int32)"),
        ]);
        let result = compare(&pair(), Some(&source), Some(&target));
        assert_eq!(result.status.exit_code(), 2);
        assert_eq!(result.findings.len(), 3);
        assert_eq!(result.findings[0].status, Status::Mismatch);
        assert_eq!(result.findings[1].status, Status::Unknown);
        assert_eq!(result.findings[2].status, Status::Compatible);
    }

    #[test]
    fn geo_information_retains_source_constraints_without_causing_drift() {
        let mut column = source_column("location", true);
        column.data_type.schema = "spatial".into();
        column.data_type.name = "geometry".into();
        column.data_type.definition = "spatial.geometry(PointZM,4326)".into();
        column.data_type.extension = Some("postgis".into());
        column.data_type.geo = Some(GeoMetadata {
            family: "geometry".into(),
            subtype: Some("PointZM".into()),
            srid: Some(4326),
            dimensions: Some(4),
            has_z: Some(true),
            has_m: Some(true),
        });
        let result = compare(
            &pair(),
            Some(&source(vec![column.clone()])),
            Some(&target(vec![target_column("location", "Nullable(String)")])),
        );
        assert_eq!(result.status, Status::Match);
        assert_eq!(result.findings.len(), 2);
        assert_eq!(result.findings[0].code, "not_represented_in_target_schema");
        assert_eq!(result.findings[0].source, Some(column));
        assert_eq!(
            result.findings[0].expected.as_deref(),
            Some("Nullable(String)")
        );
    }
}
