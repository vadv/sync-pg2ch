//! Apply explicit exclusions to the tables discovered in ClickHouse.
use serde::Serialize;

use crate::config::{DiscoveryConfig, TargetTable};

/// A discovered table excluded by one configured rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedTable {
    /// Exact destination identifier.
    pub target: TargetTable,
    /// Configuration rule that excluded this table.
    pub rule: String,
}

pub(crate) fn select(
    tables: Vec<TargetTable>,
    config: &DiscoveryConfig,
) -> (Vec<TargetTable>, Vec<SkippedTable>) {
    let mut selected = Vec::new();
    let mut skipped = Vec::new();
    for target in tables {
        let mut rule = None;
        for name in &config.exclude_tables {
            if target.table == *name {
                rule = Some(format!("exclude_tables: {name:?}"));
                break;
            }
        }
        if rule.is_none() {
            for prefix in &config.exclude_table_prefixes {
                if target.table.starts_with(prefix) {
                    rule = Some(format!("exclude_table_prefixes: {prefix:?}"));
                    break;
                }
            }
        }
        match rule {
            Some(rule) => skipped.push(SkippedTable { target, rule }),
            None => selected.push(target),
        }
    }
    (selected, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_are_literal_and_preserve_the_selected_scope() {
        let config = DiscoveryConfig {
            exclude_tables: vec!["orders_resync".into()],
            exclude_table_prefixes: vec!["_peerdb_raw_".into()],
        };
        let names = [
            "_peerdb_raw_mirror",
            "orders",
            "orders_resync",
            "orders_resync_real",
            "xpeerdb_raw_mirror",
            "Orders_resync",
        ];
        let mut tables = Vec::new();
        for name in names {
            tables.push(TargetTable {
                database: "db".into(),
                table: name.into(),
            });
        }
        let (selected, skipped) = select(tables, &config);
        assert_eq!(selected.len(), 4);
        assert_eq!(skipped.len(), 2);
        assert_eq!(skipped[0].target.table, "_peerdb_raw_mirror");
        assert_eq!(skipped[1].target.table, "orders_resync");
        assert_eq!(selected[1].table, "orders_resync_real");
    }
}
