//! PostgreSQL catalog queries, independent of access to user table rows.
use std::{str::FromStr, time::Duration};

use sqlx::{
    ConnectOptions, PgConnection, Row,
    postgres::{PgConnectOptions, PgSslMode},
};

use super::{CatalogError, Extension};
use crate::{
    config::SourceTable,
    model::{GeoMetadata, PgColumn, PgTable, PgType},
};

/// A single PostgreSQL connection used for sequential catalog reads.
pub struct PostgresCatalog {
    connection: PgConnection,
}

impl PostgresCatalog {
    /// Connect with a deadline and disable driver statement logging.
    pub async fn connect(url: &str, deadline: Duration) -> Result<Self, CatalogError> {
        let parsed = url::Url::parse(url).map_err(|_| {
            CatalogError::new("invalid_postgres_url", "invalid PostgreSQL connection URL")
        })?;
        let mut ssl_mode = PgSslMode::VerifyFull;
        for (name, value) in parsed.query_pairs() {
            if name == "sslmode" {
                ssl_mode = match value.as_ref() {
                    "disable" => PgSslMode::Disable,
                    "verify-full" => PgSslMode::VerifyFull,
                    _ => {
                        return Err(CatalogError::new(
                            "invalid_postgres_tls_mode",
                            "use sslmode=verify-full for verified TLS or sslmode=disable for explicit plaintext",
                        ));
                    }
                };
            }
        }
        let options = PgConnectOptions::from_str(url)
            .map_err(|_| {
                CatalogError::new("invalid_postgres_url", "invalid PostgreSQL connection URL")
            })?
            .ssl_mode(ssl_mode)
            .options([("default_transaction_read_only", "on")])
            .application_name("sync-pg2ch")
            .disable_statement_logging();
        let connection = tokio::time::timeout(deadline, options.connect())
            .await
            .map_err(|_| {
                CatalogError::new(
                    "postgres_connect_timeout",
                    "PostgreSQL connection timed out",
                )
            })?
            .map_err(CatalogError::pg)?;
        Ok(Self { connection })
    }

    /// Read the server version and extension inventory.
    pub async fn versions(&mut self) -> Result<(String, Vec<Extension>), CatalogError> {
        let version: String = sqlx::query_scalar("SELECT current_setting('server_version')")
            .fetch_one(&mut self.connection)
            .await
            .map_err(CatalogError::pg)?;
        let rows = sqlx::query("SELECT e.extname, e.extversion, n.nspname FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace ORDER BY e.extname")
            .fetch_all(&mut self.connection).await.map_err(CatalogError::pg)?;
        let mut extensions = Vec::new();
        for row in rows {
            extensions.push(Extension {
                name: row.try_get("extname").map_err(CatalogError::pg)?,
                version: row.try_get("extversion").map_err(CatalogError::pg)?,
                schema: row.try_get("nspname").map_err(CatalogError::pg)?,
            });
        }
        Ok((version, extensions))
    }

    /// Find literal and PeerDB-default source names within the configured schema.
    ///
    /// All matching relations are returned so that collisions cannot select a source silently.
    pub async fn source_candidates(
        &mut self,
        schema: &str,
        target: &str,
    ) -> Result<Vec<SourceTable>, CatalogError> {
        let names = source_names(schema, target);
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname::text = ANY($2) ORDER BY c.relname COLLATE \"C\"",
        )
        .bind(schema)
        .bind(&names)
        .fetch_all(&mut self.connection)
        .await
        .map_err(CatalogError::pg)?;
        let mut candidates = Vec::with_capacity(names.len());
        for table in names {
            candidates.push(SourceTable {
                schema: schema.to_owned(),
                table,
            });
        }
        Ok(candidates)
    }

    /// Read a selected relation and its visible, non-dropped columns.
    ///
    /// Catalog access failures are errors; only a successful empty lookup means absence.
    pub async fn table(&mut self, table: &SourceTable) -> Result<Option<PgTable>, CatalogError> {
        let relation = sqlx::query("SELECT c.oid::bigint AS oid, c.relkind::text AS kind, c.relpersistence::text AS persistence FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2")
            .bind(&table.schema).bind(&table.table)
            .fetch_optional(&mut self.connection).await.map_err(CatalogError::pg)?;
        let Some(relation) = relation else {
            return Ok(None);
        };
        let oid: i64 = relation.try_get("oid").map_err(CatalogError::pg)?;
        let kind = relation.try_get("kind").map_err(CatalogError::pg)?;
        let persistence = relation.try_get("persistence").map_err(CatalogError::pg)?;
        let rows = sqlx::query(
            r#"
            SELECT a.attname, a.attnotnull, a.attgenerated::text AS generated,
                   a.atttypmod, t.typname, tn.nspname AS type_schema, t.typtype::text AS type_kind,
                   pg_catalog.format_type(a.atttypid, a.atttypmod) AS definition,
                   e.extname AS extension, en.nspname AS extension_schema
            FROM pg_catalog.pg_attribute a
            JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
            JOIN pg_catalog.pg_namespace tn ON tn.oid = t.typnamespace
            LEFT JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_type'::regclass
                AND d.objid = t.oid AND d.objsubid = 0 AND d.deptype = 'e'
                AND d.refclassid = 'pg_catalog.pg_extension'::regclass
            LEFT JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid
            LEFT JOIN pg_catalog.pg_namespace en ON en.oid = e.extnamespace
            WHERE a.attrelid = $1::bigint::oid AND a.attnum > 0 AND NOT a.attisdropped
            ORDER BY a.attname COLLATE "C"
        "#,
        )
        .bind(oid)
        .fetch_all(&mut self.connection)
        .await
        .map_err(CatalogError::pg)?;
        let mut columns = Vec::with_capacity(rows.len());
        for row in rows {
            let mut data_type = PgType {
                schema: row.try_get("type_schema").map_err(CatalogError::pg)?,
                name: row.try_get("typname").map_err(CatalogError::pg)?,
                definition: row.try_get("definition").map_err(CatalogError::pg)?,
                kind: row.try_get("type_kind").map_err(CatalogError::pg)?,
                typmod: row.try_get("atttypmod").map_err(CatalogError::pg)?,
                extension: row.try_get("extension").map_err(CatalogError::pg)?,
                geo: None,
            };
            if data_type.extension.as_deref() == Some("postgis")
                && (data_type.name == "geometry" || data_type.name == "geography")
            {
                let schema: String = row.try_get("extension_schema").map_err(CatalogError::pg)?;
                data_type.geo = Some(self.geo_metadata(&schema, &data_type).await?);
            } else if data_type.schema == "pg_catalog"
                && ["point", "line", "lseg", "box", "path", "polygon", "circle"]
                    .contains(&data_type.name.as_str())
            {
                data_type.geo = Some(GeoMetadata {
                    family: "pg_catalog".into(),
                    subtype: None,
                    srid: None,
                    dimensions: Some(2),
                    has_z: Some(false),
                    has_m: Some(false),
                });
            }
            let generated: String = row.try_get("generated").map_err(CatalogError::pg)?;
            let not_null: bool = row.try_get("attnotnull").map_err(CatalogError::pg)?;
            columns.push(PgColumn {
                name: row.try_get("attname").map_err(CatalogError::pg)?,
                data_type,
                nullable: !not_null,
                generated: !generated.is_empty(),
            });
        }
        Ok(Some(PgTable {
            oid,
            kind,
            persistence,
            columns,
        }))
    }

    async fn geo_metadata(
        &mut self,
        schema: &str,
        data_type: &PgType,
    ) -> Result<GeoMetadata, CatalogError> {
        let mut geo = GeoMetadata {
            family: data_type.name.clone(),
            subtype: None,
            srid: None,
            dimensions: None,
            has_z: None,
            has_m: None,
        };
        if data_type.typmod < 0 {
            return Ok(geo);
        }
        let schema = quote_identifier(schema);
        let query = format!(
            "SELECT {schema}.postgis_typmod_type($1)::text AS subtype, {schema}.postgis_typmod_srid($1)::integer AS srid, {schema}.postgis_typmod_dims($1)::integer AS dimensions"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(data_type.typmod)
            .fetch_one(&mut self.connection)
            .await
            .map_err(CatalogError::pg)?;
        let subtype: String = row.try_get("subtype").map_err(CatalogError::pg)?;
        let dimensions: i32 = row.try_get("dimensions").map_err(CatalogError::pg)?;
        let has_z = subtype.ends_with('Z') || subtype.ends_with("ZM");
        let has_m = subtype.ends_with('M');
        let base = subtype
            .trim_end_matches('M')
            .trim_end_matches('Z')
            .to_owned();
        geo.subtype = Some(base);
        geo.srid = Some(row.try_get("srid").map_err(CatalogError::pg)?);
        geo.dimensions = Some(u8::try_from(dimensions).map_err(|_| {
            CatalogError::new(
                "invalid_postgis_metadata",
                "PostGIS returned an invalid dimension",
            )
        })?);
        geo.has_z = Some(has_z);
        geo.has_m = Some(has_m);
        Ok(geo)
    }
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Return literal source names allowed by exact and PeerDB-default matching.
pub(crate) fn source_names<'a>(schema: &str, target: &'a str) -> Vec<&'a str> {
    let mut names = vec![target];
    if let Some(table) = target.strip_prefix(&format!("{schema}_"))
        && !table.is_empty()
    {
        names.push(table);
    }
    names
}

#[cfg(test)]
mod tests {
    #[test]
    fn peerdb_prefix_is_literal_and_uses_the_entire_schema_name() {
        for (schema, target, expected) in [
            ("public", "orders", vec!["orders"]),
            ("public", "public_orders", vec!["public_orders", "orders"]),
            (
                "sales_eu",
                "sales_eu_order_items",
                vec!["sales_eu_order_items", "order_items"],
            ),
            (
                "public",
                "public__orders",
                vec!["public__orders", "_orders"],
            ),
            ("public", "public_", vec!["public_"]),
            ("public", "public.orders", vec!["public.orders"]),
            ("public", "Public_orders", vec!["Public_orders"]),
            ("a.\"Ю", "a.\"Ю_Table", vec!["a.\"Ю_Table", "Table"]),
        ] {
            assert_eq!(super::source_names(schema, target), expected);
        }
    }

    #[test]
    fn extension_namespace_is_quoted_as_one_identifier() {
        assert_eq!(
            super::quote_identifier("spatial.\"odd"),
            "\"spatial.\"\"odd\""
        );
    }
}
