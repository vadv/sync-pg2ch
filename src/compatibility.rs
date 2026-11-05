//! Directed PostgreSQL to ClickHouse type rules for `pg-clickhouse-v1`.

use crate::model::{PgType, Status};

const MAX_TYPE_LENGTH: usize = 4096;
const MAX_TYPE_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Scalar {
    Signed(u8),
    Unsigned(u8),
    Float(u8),
    Bool,
    String,
    FixedString(u32),
    Uuid,
    Decimal {
        precision: u8,
        scale: u8,
    },
    Date,
    Date32,
    DateTime(Option<String>),
    DateTime64 {
        precision: u8,
        timezone: Option<String>,
    },
    Geo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Scalar(Scalar),
    Nullable(Box<Node>),
    LowCardinality(Box<Node>),
    Tuple(Vec<Node>),
    Array(Box<Node>),
}

#[derive(Debug, PartialEq, Eq)]
struct Target {
    scalar: Scalar,
    nullable: bool,
    low_cardinality: bool,
}

/// Compare normalized CH types without losing unsupported but unchanged definitions.
pub(crate) fn same_target_type(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (parse_target(left), parse_target(right)) {
        (Ok(left), Ok(right)) => left.scalar != Scalar::Geo && left == right,
        _ => false,
    }
}

#[derive(Debug)]
struct Expected {
    scalar: Scalar,
    definition: String,
    allow_low_cardinality: bool,
    geo: bool,
    decimal_requirement: Option<(u8, u8)>,
}

/// Type-level evaluation used by the table comparator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeComparison {
    pub(crate) status: Status,
    pub(crate) code: &'static str,
    pub(crate) expected: Option<String>,
    pub(crate) target_nullable: Option<bool>,
    pub(crate) geo: bool,
    pub(crate) message: String,
}

/// Returns the canonical target representation without inspecting a target column.
pub(crate) fn expected_definition(source: &PgType) -> Result<String, String> {
    let expected = expected_type(source)?;
    Ok(expected.definition)
}

/// Choose an ALTER destination that retains both existing CH values and PG requirements.
pub(crate) fn alter_destination(
    source: &PgType,
    source_nullable: bool,
    current: &str,
    default_timezone: Option<&str>,
) -> Result<String, String> {
    let comparison = compare_types_in_timezone(source, current, default_timezone);
    if comparison.status == Status::Unknown {
        return Err(comparison.message);
    }
    let expected = expected_type(source)?;
    let mut target = parse_target(current)?;
    if comparison.status == Status::Mismatch {
        target.scalar = match (&expected.scalar, &target.scalar) {
            (Scalar::Signed(required), Scalar::Signed(existing)) => {
                Scalar::Signed((*required).max(*existing))
            }
            (Scalar::Float(64), Scalar::Float(32)) => Scalar::Float(64),
            (Scalar::Date32, Scalar::Date) => Scalar::Date32,
            (Scalar::Decimal { .. }, Scalar::Decimal { precision, scale }) => {
                let (required_precision, required_scale) = expected
                    .decimal_requirement
                    .ok_or("numeric requirements are missing")?;
                let new_scale = (*scale).max(required_scale);
                let integer_digits = (precision - scale).max(required_precision - required_scale);
                let precision = integer_digits
                    .checked_add(new_scale)
                    .filter(|precision| *precision <= 76)
                    .ok_or("no Decimal type retains both existing CH values and PG requirements")?;
                Scalar::Decimal {
                    precision: decimal_capacity(precision),
                    scale: new_scale,
                }
            }
            _ => return Err("no lossless ALTER rule for this type conversion".into()),
        };
    }
    target.nullable |= source_nullable;
    let mut definition = scalar_definition(&target.scalar);
    if target.nullable {
        definition = format!("Nullable({definition})");
    }
    if target.low_cardinality {
        definition = format!("LowCardinality({definition})");
    }
    let comparison = compare_types_in_timezone(source, &definition, default_timezone);
    if comparison.status > Status::Compatible {
        return Err(comparison.message);
    }
    Ok(definition)
}

/// Compares a catalog-resolved PostgreSQL type with a ClickHouse type expression.
#[cfg(test)]
pub(crate) fn compare_types(source: &PgType, target: &str) -> TypeComparison {
    compare_types_in_timezone(source, target, None)
}

/// Evaluate implicit CH timestamps using the timezone observed in the catalog snapshot.
pub(crate) fn compare_types_in_timezone(
    source: &PgType,
    target: &str,
    default_timezone: Option<&str>,
) -> TypeComparison {
    let expected = expected_type(source);
    let target = parse_target(target);
    let expected = match expected {
        Ok(expected) => expected,
        Err(message) => {
            return TypeComparison {
                status: Status::Unknown,
                code: "unsupported_source_type",
                expected: None,
                target_nullable: target.ok().map(|target| target.nullable),
                geo: false,
                message,
            };
        }
    };
    let target = match target {
        Ok(target) => target,
        Err(message) => {
            return TypeComparison {
                status: Status::Unknown,
                code: "unsupported_target_type",
                expected: Some(expected.definition),
                target_nullable: None,
                geo: expected.geo,
                message,
            };
        }
    };
    let (status, code, message) = compare_scalar(&expected, &target.scalar, default_timezone);
    let (status, code, message) = if target.low_cardinality {
        if expected.allow_low_cardinality && target.scalar == Scalar::String {
            (
                status.max(Status::Compatible),
                if status == Status::Match {
                    "type_widening"
                } else {
                    code
                },
                format!("{message}; LowCardinality is an allowed string storage wrapper"),
            )
        } else {
            (
                Status::Unknown,
                "unsupported_target_wrapper",
                "LowCardinality is supported only for the string mappings listed by this policy"
                    .into(),
            )
        }
    } else {
        (status, code, message)
    };
    TypeComparison {
        status,
        code,
        expected: Some(expected.definition),
        target_nullable: Some(target.nullable),
        geo: expected.geo,
        message,
    }
}

fn expected_type(source: &PgType) -> Result<Expected, String> {
    if source.kind != "b" {
        return Err(format!(
            "PostgreSQL type kind {:?} has no v1 mapping",
            source.kind
        ));
    }
    if source.extension.as_deref() == Some("postgis")
        && (source.name == "geometry" || source.name == "geography")
    {
        return Ok(string_expected(true, true));
    }
    if source.schema != "pg_catalog" || source.extension.is_some() {
        return Err(
            "only catalog built-ins and PostGIS-owned geometry/geography have v1 mappings".into(),
        );
    }
    let scalar = match source.name.as_str() {
        "int2" => Scalar::Signed(16),
        "int4" => Scalar::Signed(32),
        "int8" => Scalar::Signed(64),
        "float4" => Scalar::Float(32),
        "float8" => Scalar::Float(64),
        "bool" => Scalar::Bool,
        "text" | "varchar" | "json" | "jsonb" | "tsvector" => {
            return Ok(string_expected(true, false));
        }
        "bytea" => return Ok(string_expected(false, false)),
        "point" | "line" | "lseg" | "box" | "path" | "polygon" | "circle" => {
            return Ok(string_expected(true, true));
        }
        "uuid" => Scalar::Uuid,
        "date" => Scalar::Date32,
        "numeric" => return expected_numeric(source.typmod),
        "timestamp" | "timestamptz" => {
            let precision = if source.typmod == -1 {
                6
            } else {
                source.typmod
            };
            let Ok(precision) = u8::try_from(precision) else {
                return Err("invalid PostgreSQL timestamp precision".into());
            };
            if precision > 6 {
                return Err("PostgreSQL timestamp precision is outside the v1 range 0..=6".into());
            }
            Scalar::DateTime64 {
                precision,
                timezone: if source.name == "timestamptz" {
                    Some("UTC".into())
                } else {
                    None
                },
            }
        }
        name => return Err(format!("PostgreSQL type {name:?} has no v1 mapping")),
    };
    Ok(Expected {
        definition: scalar_definition(&scalar),
        scalar,
        allow_low_cardinality: false,
        geo: false,
        decimal_requirement: None,
    })
}

fn string_expected(allow_low_cardinality: bool, geo: bool) -> Expected {
    Expected {
        scalar: Scalar::String,
        definition: "String".into(),
        allow_low_cardinality,
        geo,
        decimal_requirement: None,
    }
}

fn expected_numeric(typmod: i32) -> Result<Expected, String> {
    let Some(typmod) = typmod.checked_sub(4) else {
        return Err("invalid PostgreSQL numeric modifier".into());
    };
    if typmod < 0 {
        return Err("numeric without precision and scale has no bounded v1 mapping".into());
    }
    let precision = (typmod >> 16) & 0xffff;
    let scale = typmod & 0xffff;
    if !(1..=76).contains(&precision) || scale > precision {
        return Err("numeric precision/scale is outside the supported ClickHouse range".into());
    }
    let precision = u8::try_from(precision).map_err(|error| error.to_string())?;
    let scale = u8::try_from(scale).map_err(|error| error.to_string())?;
    let scalar = Scalar::Decimal {
        precision: decimal_capacity(precision),
        scale,
    };
    Ok(Expected {
        definition: scalar_definition(&scalar),
        scalar,
        allow_low_cardinality: false,
        geo: false,
        decimal_requirement: Some((precision, scale)),
    })
}

fn decimal_capacity(precision: u8) -> u8 {
    if precision <= 9 {
        9
    } else if precision <= 18 {
        18
    } else if precision <= 38 {
        38
    } else {
        76
    }
}

fn scalar_definition(scalar: &Scalar) -> String {
    match scalar {
        Scalar::Signed(bits) => format!("Int{bits}"),
        Scalar::Unsigned(bits) => format!("UInt{bits}"),
        Scalar::Float(bits) => format!("Float{bits}"),
        Scalar::Bool => "Bool".into(),
        Scalar::String => "String".into(),
        Scalar::FixedString(length) => format!("FixedString({length})"),
        Scalar::Uuid => "UUID".into(),
        Scalar::Decimal { precision, scale } => format!("Decimal({precision},{scale})"),
        Scalar::Date => "Date".into(),
        Scalar::Date32 => "Date32".into(),
        Scalar::DateTime(timezone) => match timezone {
            Some(timezone) => format!(
                "DateTime('{}')",
                timezone.replace('\\', "\\\\").replace('\'', "\\'")
            ),
            None => "DateTime".into(),
        },
        Scalar::DateTime64 {
            precision,
            timezone,
        } => match timezone {
            Some(timezone) => format!(
                "DateTime64({precision}, '{}')",
                timezone.replace('\\', "\\\\").replace('\'', "\\'")
            ),
            None => format!("DateTime64({precision})"),
        },
        Scalar::Geo => "native geometry".into(),
    }
}

fn compare_scalar(
    expected: &Expected,
    target: &Scalar,
    default_timezone: Option<&str>,
) -> (Status, &'static str, String) {
    if let Scalar::DateTime64 {
        precision,
        timezone: _,
    } = target
        && *precision > 6
    {
        return (
            Status::Unknown,
            "unsupported_target_precision",
            "DateTime64 precision 7..=9 has no v1 range-preserving rule".into(),
        );
    }
    if expected.scalar == *target {
        return (
            Status::Match,
            "type_match",
            "type matches the canonical source mapping".into(),
        );
    }
    if expected.geo && *target == Scalar::Geo {
        return (Status::Mismatch, "geo_representation_mismatch", "the policy requires serialized geometry in String; native ClickHouse geometry requires a separate conversion rule".into());
    }
    let widening = match &expected.scalar {
        Scalar::Signed(source_bits) => match target {
            Scalar::Signed(target_bits) => target_bits > source_bits,
            Scalar::Unsigned(_)
            | Scalar::Float(_)
            | Scalar::Bool
            | Scalar::String
            | Scalar::FixedString(_)
            | Scalar::Uuid
            | Scalar::Decimal { .. }
            | Scalar::Date
            | Scalar::Date32
            | Scalar::DateTime(_)
            | Scalar::DateTime64 { .. }
            | Scalar::Geo => false,
        },
        Scalar::Float(source_bits) => match target {
            Scalar::Float(target_bits) => target_bits > source_bits,
            Scalar::Signed(_)
            | Scalar::Unsigned(_)
            | Scalar::Bool
            | Scalar::String
            | Scalar::FixedString(_)
            | Scalar::Uuid
            | Scalar::Decimal { .. }
            | Scalar::Date
            | Scalar::Date32
            | Scalar::DateTime(_)
            | Scalar::DateTime64 { .. }
            | Scalar::Geo => false,
        },
        Scalar::Decimal { .. } => {
            let Some((source_precision, source_scale)) = expected.decimal_requirement else {
                return (
                    Status::Unknown,
                    "invalid_source_metadata",
                    "numeric source precision/scale is missing".into(),
                );
            };
            match target {
                Scalar::Decimal { precision, scale } => {
                    *scale >= source_scale && precision - scale >= source_precision - source_scale
                }
                Scalar::Signed(_)
                | Scalar::Unsigned(_)
                | Scalar::Float(_)
                | Scalar::Bool
                | Scalar::String
                | Scalar::FixedString(_)
                | Scalar::Uuid
                | Scalar::Date
                | Scalar::Date32
                | Scalar::DateTime(_)
                | Scalar::DateTime64 { .. }
                | Scalar::Geo => false,
            }
        }
        Scalar::DateTime64 {
            precision: source_precision,
            timezone: source_timezone,
        } => match target {
            Scalar::DateTime64 {
                precision,
                timezone,
            } => {
                if timezone.is_none() && source_timezone.as_deref() == Some("UTC") {
                    match default_timezone {
                        Some("UTC" | "Etc/UTC") if precision >= source_precision => return (
                            Status::Compatible,
                            "inherited_timezone",
                            "the target inherits UTC from this ClickHouse connection; other sessions may override the implicit timezone".into(),
                        ),
                        Some("UTC" | "Etc/UTC") => return (Status::Mismatch, "type_mismatch", "target timestamp precision is narrower than the source".into()),
                        Some(zone) => return (Status::Mismatch, "timezone_mismatch", format!("target inherits timezone {zone:?}; source mapping requires UTC")),
                        None => return (Status::Unknown, "implicit_timezone_unknown", "the target has no explicit timezone and the ClickHouse default timezone is unavailable".into()),
                    }
                }
                if timezone != source_timezone {
                    return (
                        Status::Mismatch,
                        "timezone_mismatch",
                        "explicit timezone does not match the source mapping".into(),
                    );
                }
                precision > source_precision
            }
            Scalar::Signed(_)
            | Scalar::Unsigned(_)
            | Scalar::Float(_)
            | Scalar::Bool
            | Scalar::String
            | Scalar::FixedString(_)
            | Scalar::Uuid
            | Scalar::Decimal { .. }
            | Scalar::Date
            | Scalar::Date32
            | Scalar::DateTime(_)
            | Scalar::Geo => false,
        },
        Scalar::Unsigned(_)
        | Scalar::Bool
        | Scalar::String
        | Scalar::FixedString(_)
        | Scalar::Uuid
        | Scalar::Date
        | Scalar::Date32
        | Scalar::DateTime(_)
        | Scalar::Geo => false,
    };
    if widening {
        (
            Status::Compatible,
            "type_widening",
            "target retains the source capacity required by the v1 mapping".into(),
        )
    } else {
        (
            Status::Mismatch,
            "type_mismatch",
            format!(
                "{} does not satisfy the source mapping {}",
                scalar_definition(target),
                expected.definition
            ),
        )
    }
}

fn parse_target(input: &str) -> Result<Target, String> {
    if input.len() > MAX_TYPE_LENGTH {
        return Err("ClickHouse type expression exceeds the parser length limit".into());
    }
    let mut parser = Parser { input, position: 0 };
    let node = parser.node(0)?;
    parser.whitespace();
    if parser.position != input.len() {
        return Err("unexpected trailing text in ClickHouse type expression".into());
    }
    let mut nullable = false;
    let mut low_cardinality = false;
    let mut node = &node;
    loop {
        match node {
            Node::Nullable(inner) => {
                if nullable {
                    return Err("duplicate Nullable wrapper".into());
                }
                nullable = true;
                node = inner;
            }
            Node::LowCardinality(inner) => {
                if low_cardinality {
                    return Err("duplicate LowCardinality wrapper".into());
                }
                if nullable {
                    return Err("Nullable cannot contain LowCardinality; put Nullable inside LowCardinality".into());
                }
                low_cardinality = true;
                node = inner;
            }
            Node::Scalar(scalar) => {
                return Ok(Target {
                    scalar: scalar.clone(),
                    nullable,
                    low_cardinality,
                });
            }
            Node::Tuple(_) | Node::Array(_) => {
                if nullable && let Node::Array(_) = node {
                    return Err("ClickHouse Nullable cannot contain an Array".into());
                }
                if geo_shape(node).is_some() {
                    return Ok(Target {
                        scalar: Scalar::Geo,
                        nullable,
                        low_cardinality,
                    });
                }
                return Err("ClickHouse composite type has no v1 mapping".into());
            }
        }
    }
}

fn geo_shape(node: &Node) -> Option<u8> {
    match node {
        Node::Tuple(elements) => {
            if elements.as_slice()
                == [
                    Node::Scalar(Scalar::Float(64)),
                    Node::Scalar(Scalar::Float(64)),
                ]
            {
                Some(0)
            } else {
                None
            }
        }
        Node::Array(inner) => match geo_shape(inner) {
            Some(depth) if depth < 3 => Some(depth + 1),
            Some(_) | None => None,
        },
        Node::Nullable(_) | Node::LowCardinality(_) | Node::Scalar(_) => None,
    }
}

fn native_geo_node(array_depth: u8) -> Node {
    let mut node = Node::Tuple(vec![
        Node::Scalar(Scalar::Float(64)),
        Node::Scalar(Scalar::Float(64)),
    ]);
    for _ in 0..array_depth {
        node = Node::Array(Box::new(node));
    }
    node
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn whitespace(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        self.whitespace();
        if self.input.as_bytes().get(self.position) == Some(&byte) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn require(&mut self, byte: u8) -> Result<(), String> {
        if self.consume(byte) {
            Ok(())
        } else {
            Err(format!(
                "expected {:?} in ClickHouse type",
                char::from(byte)
            ))
        }
    }

    fn word(&mut self) -> Result<&str, String> {
        self.whitespace();
        let start = self.position;
        while let Some(byte) = self.input.as_bytes().get(self.position) {
            if !byte.is_ascii_alphanumeric() && *byte != b'_' {
                break;
            }
            self.position += 1;
        }
        if start == self.position {
            return Err("expected a ClickHouse type name".into());
        }
        Ok(&self.input[start..self.position])
    }

    fn number(&mut self) -> Result<u32, String> {
        self.whitespace();
        let start = self.position;
        while self
            .input
            .as_bytes()
            .get(self.position)
            .is_some_and(u8::is_ascii_digit)
        {
            self.position += 1;
        }
        self.input[start..self.position]
            .parse()
            .map_err(|error| format!("invalid type parameter: {error}"))
    }

    fn quoted(&mut self) -> Result<String, String> {
        self.require(b'\'')?;
        let mut result = Vec::new();
        loop {
            let Some(byte) = self.input.as_bytes().get(self.position).copied() else {
                return Err("unterminated timezone string".into());
            };
            self.position += 1;
            if byte == b'\\' {
                let Some(byte) = self.input.as_bytes().get(self.position).copied() else {
                    return Err("unterminated timezone escape".into());
                };
                self.position += 1;
                if byte != b'\\' && byte != b'\'' {
                    return Err("unsupported timezone escape".into());
                }
                result.push(byte);
            } else if byte == b'\'' {
                if self.input.as_bytes().get(self.position) == Some(&b'\'') {
                    self.position += 1;
                    result.push(byte);
                } else {
                    return String::from_utf8(result).map_err(|error| error.to_string());
                }
            } else {
                result.push(byte);
            }
        }
    }

    fn node(&mut self, depth: usize) -> Result<Node, String> {
        if depth >= MAX_TYPE_DEPTH {
            return Err("ClickHouse type nesting exceeds the parser limit".into());
        }
        let name = self.word()?.to_owned();
        let scalar = match name.as_str() {
            "Int8" => Scalar::Signed(8),
            "Int16" => Scalar::Signed(16),
            "Int32" => Scalar::Signed(32),
            "Int64" => Scalar::Signed(64),
            "UInt8" => Scalar::Unsigned(8),
            "UInt16" => Scalar::Unsigned(16),
            "UInt32" => Scalar::Unsigned(32),
            "UInt64" => Scalar::Unsigned(64),
            "Float32" => Scalar::Float(32),
            "Float64" => Scalar::Float(64),
            "Bool" | "Boolean" => Scalar::Bool,
            "String" => Scalar::String,
            "UUID" => Scalar::Uuid,
            "Date" => Scalar::Date,
            "Date32" => Scalar::Date32,
            "Point" => return Ok(native_geo_node(0)),
            "Ring" | "LineString" => return Ok(native_geo_node(1)),
            "MultiLineString" | "Polygon" => return Ok(native_geo_node(2)),
            "MultiPolygon" => return Ok(native_geo_node(3)),
            "Geometry" => Scalar::Geo,
            "Nullable" | "LowCardinality" | "Array" => {
                self.require(b'(')?;
                let inner = Box::new(self.node(depth + 1)?);
                self.require(b')')?;
                return Ok(if name == "Nullable" {
                    Node::Nullable(inner)
                } else if name == "LowCardinality" {
                    Node::LowCardinality(inner)
                } else {
                    Node::Array(inner)
                });
            }
            "Tuple" => {
                self.require(b'(')?;
                let mut elements = Vec::new();
                if !self.consume(b')') {
                    loop {
                        elements.push(self.node(depth + 1)?);
                        if self.consume(b')') {
                            break;
                        }
                        self.require(b',')?;
                    }
                }
                return Ok(Node::Tuple(elements));
            }
            "FixedString" => {
                self.require(b'(')?;
                let length = self.number()?;
                self.require(b')')?;
                if length == 0 {
                    return Err("FixedString length must be positive".into());
                }
                Scalar::FixedString(length)
            }
            "Decimal" | "Decimal32" | "Decimal64" | "Decimal128" | "Decimal256" => {
                self.require(b'(')?;
                let (precision, scale) = if name == "Decimal" {
                    let precision = self.number()?;
                    self.require(b',')?;
                    (precision, self.number()?)
                } else {
                    let precision = if name == "Decimal32" {
                        9
                    } else if name == "Decimal64" {
                        18
                    } else if name == "Decimal128" {
                        38
                    } else {
                        76
                    };
                    (precision, self.number()?)
                };
                self.require(b')')?;
                if precision == 0 || precision > 76 || scale > precision {
                    return Err("invalid ClickHouse decimal precision/scale".into());
                }
                let precision = u8::try_from(precision).map_err(|error| error.to_string())?;
                let scale = u8::try_from(scale).map_err(|error| error.to_string())?;
                Scalar::Decimal {
                    precision: decimal_capacity(precision),
                    scale,
                }
            }
            "DateTime" => {
                let timezone = if self.consume(b'(') {
                    let timezone = self.quoted()?;
                    self.require(b')')?;
                    Some(timezone)
                } else {
                    None
                };
                Scalar::DateTime(timezone)
            }
            "DateTime64" => {
                self.require(b'(')?;
                let precision = self.number()?;
                if precision > 9 {
                    return Err("DateTime64 precision must be between 0 and 9".into());
                }
                let timezone = if self.consume(b',') {
                    Some(self.quoted()?)
                } else {
                    None
                };
                self.require(b')')?;
                Scalar::DateTime64 {
                    precision: u8::try_from(precision).map_err(|error| error.to_string())?,
                    timezone,
                }
            }
            name => {
                return Err(format!(
                    "ClickHouse type {name:?} has no supported v1 parser rule"
                ));
            }
        };
        Ok(Node::Scalar(scalar))
    }
}

#[cfg(test)]
mod tests {
    use crate::model::{GeoMetadata, PgType, Status};

    use super::{Scalar, alter_destination, compare_types, parse_target, same_target_type};

    fn pg(name: &str, typmod: i32) -> PgType {
        PgType {
            schema: "pg_catalog".into(),
            name: name.into(),
            definition: name.into(),
            kind: "b".into(),
            typmod,
            extension: None,
            geo: None,
        }
    }

    #[test]
    fn implicit_utc_does_not_hide_precision_or_nullability_requirements() {
        use super::compare_types_in_timezone;
        let source = pg("timestamptz", 6);
        assert_eq!(
            compare_types_in_timezone(&source, "DateTime64(6)", Some("UTC")).status,
            Status::Compatible
        );
        assert_eq!(
            compare_types_in_timezone(&source, "DateTime64(6)", Some("Europe/Berlin")).status,
            Status::Mismatch
        );
        assert_eq!(
            compare_types_in_timezone(&source, "DateTime64(3)", Some("UTC")).status,
            Status::Mismatch
        );
        assert_eq!(
            compare_types_in_timezone(&source, "DateTime64(9)", Some("UTC")).status,
            Status::Unknown
        );
        assert_eq!(
            compare_types_in_timezone(&source, "DateTime64(6, 'Europe/Berlin')", Some("UTC"))
                .status,
            Status::Mismatch
        );
        assert_eq!(
            alter_destination(&source, true, "DateTime64(6)", Some("UTC")).unwrap(),
            "Nullable(DateTime64(6))"
        );
    }

    #[test]
    fn integer_and_float_rules_are_directional() {
        for (source, target, status) in [
            ("int2", "Int16", Status::Match),
            ("int2", "Int32", Status::Compatible),
            ("int2", "Int64", Status::Compatible),
            ("int4", "Int64", Status::Compatible),
            ("int8", "Int32", Status::Mismatch),
            ("int8", "UInt64", Status::Mismatch),
            ("int2", "Int8", Status::Mismatch),
            ("int8", "Int128", Status::Unknown),
            ("int4", "Float64", Status::Mismatch),
            ("float4", "Float64", Status::Compatible),
            ("float8", "Float32", Status::Mismatch),
            ("float8", "String", Status::Mismatch),
        ] {
            assert_eq!(
                compare_types(&pg(source, -1), target).status,
                status,
                "{source} -> {target}"
            );
        }
    }

    #[test]
    fn alter_retains_existing_ch_capacity_and_wrappers() {
        assert!(same_target_type("Decimal64(4)", "Decimal(18,4)"));
        assert!(!same_target_type("Point", "Polygon"));
        for (source, nullable, current, expected) in [
            ("int4", true, "Int64", "Nullable(Int64)"),
            ("int8", false, "Nullable(Int32)", "Nullable(Int64)"),
            (
                "text",
                true,
                "LowCardinality(String)",
                "LowCardinality(Nullable(String))",
            ),
            ("float8", false, "Float32", "Float64"),
            ("date", false, "Nullable(Date)", "Nullable(Date32)"),
        ] {
            assert_eq!(
                alter_destination(&pg(source, -1), nullable, current, None).unwrap(),
                expected
            );
        }
        for (source, current) in [
            ("int8", "UInt64"),
            ("int4", "String"),
            ("uuid", "String"),
            ("timestamp", "DateTime64(0)"),
        ] {
            assert!(alter_destination(&pg(source, -1), false, current, None).is_err());
        }
        let modifier = |precision, scale| ((precision << 16) | scale) + 4;
        assert_eq!(
            alter_destination(&pg("numeric", modifier(12, 4)), false, "Decimal(9,2)", None)
                .unwrap(),
            "Decimal(18,4)"
        );
        assert_eq!(
            alter_destination(
                &pg("numeric", modifier(18, 4)),
                false,
                "Decimal(18,2)",
                None
            )
            .unwrap(),
            "Decimal(38,4)"
        );
        assert!(
            alter_destination(
                &pg("numeric", modifier(76, 10)),
                false,
                "Decimal(76,0)",
                None
            )
            .is_err()
        );
    }

    #[test]
    fn decimal_capacity_uses_original_pg_precision() {
        for (precision, scale, target, status) in [
            (10, 2, "Decimal64(2)", Status::Match),
            (10, 2, "Decimal(18,2)", Status::Match),
            (10, 2, "Decimal64(4)", Status::Compatible),
            (18, 2, "Decimal(38,4)", Status::Compatible),
            (18, 2, "Decimal(18,4)", Status::Mismatch),
            (18, 4, "Decimal(38,2)", Status::Mismatch),
            (18, 2, "Decimal32(2)", Status::Mismatch),
            (76, 38, "Decimal256(38)", Status::Match),
        ] {
            let source = pg("numeric", ((precision << 16) | scale) + 4);
            assert_eq!(
                compare_types(&source, target).status,
                status,
                "numeric({precision},{scale}) -> {target}"
            );
        }
        for modifier in [-1, 4, (77 << 16) + 4, (2 << 16) + 3 + 4, i32::MIN] {
            assert_eq!(
                compare_types(&pg("numeric", modifier), "Decimal256(38)").status,
                Status::Unknown
            );
        }
    }

    #[test]
    fn timestamp_precision_and_timezone_remain_separate_constraints() {
        for (source, precision, target, status, code) in [
            (
                "timestamp",
                -1,
                "DateTime64(6)",
                Status::Match,
                "type_match",
            ),
            (
                "timestamp",
                3,
                "DateTime64(6)",
                Status::Compatible,
                "type_widening",
            ),
            (
                "timestamptz",
                6,
                "DateTime64(3, 'UTC')",
                Status::Mismatch,
                "type_mismatch",
            ),
            (
                "timestamptz",
                6,
                "DateTime64(9, 'UTC')",
                Status::Unknown,
                "unsupported_target_precision",
            ),
            (
                "timestamptz",
                6,
                "DateTime64(6, 'Europe/Berlin')",
                Status::Mismatch,
                "timezone_mismatch",
            ),
            (
                "timestamp",
                6,
                "DateTime64(6, 'UTC')",
                Status::Mismatch,
                "timezone_mismatch",
            ),
            (
                "timestamptz",
                6,
                "DateTime64(6)",
                Status::Unknown,
                "implicit_timezone_unknown",
            ),
            (
                "timestamp",
                6,
                "DateTime",
                Status::Mismatch,
                "type_mismatch",
            ),
        ] {
            let result = compare_types(&pg(source, precision), target);
            assert_eq!(
                (result.status, result.code),
                (status, code),
                "{source} -> {target}"
            );
        }
    }

    #[test]
    fn supported_scalar_mappings_and_low_cardinality_are_explicit() {
        for (source, target) in [
            ("bool", "Bool"),
            ("bool", "Boolean"),
            ("text", "String"),
            ("varchar", "String"),
            ("uuid", "UUID"),
            ("date", "Date32"),
            ("json", "String"),
            ("jsonb", "String"),
            ("bytea", "String"),
        ] {
            assert_eq!(compare_types(&pg(source, -1), target).status, Status::Match);
            assert_eq!(
                compare_types(&pg(source, -1), "Int16").status,
                Status::Mismatch
            );
        }
        assert_eq!(
            compare_types(&pg("date", -1), "Date").status,
            Status::Mismatch
        );
        assert_eq!(
            compare_types(&pg("bool", -1), "UInt8").status,
            Status::Mismatch
        );
        assert_eq!(
            compare_types(&pg("text", -1), "LowCardinality(Nullable(String))").target_nullable,
            Some(true)
        );
        assert_eq!(
            compare_types(&pg("jsonb", -1), "LowCardinality(String)").status,
            Status::Compatible
        );
        assert_eq!(
            compare_types(&pg("bytea", -1), "LowCardinality(String)").status,
            Status::Unknown
        );
        assert_eq!(
            compare_types(&pg("int4", -1), "LowCardinality(Int32)").status,
            Status::Unknown
        );
    }

    #[test]
    fn unsupported_source_types_cannot_pass_by_name() {
        for name in ["_int4", "bpchar", "time", "interval", "geometry", "money"] {
            assert_eq!(
                compare_types(&pg(name, -1), "String").status,
                Status::Unknown
            );
        }
        let mut source = pg("int4", -1);
        source.schema = "user_types".into();
        assert_eq!(compare_types(&source, "Int32").status, Status::Unknown);
        source.schema = "pg_catalog".into();
        source.kind = "d".into();
        assert_eq!(compare_types(&source, "Int32").status, Status::Unknown);
    }

    #[test]
    fn builtin_and_extension_geometry_use_a_deliberate_string_mapping() {
        for name in ["point", "line", "lseg", "box", "path", "polygon", "circle"] {
            let result = compare_types(&pg(name, -1), "String");
            assert_eq!(result.status, Status::Match);
            assert!(result.geo);
            for target in [
                "Point",
                "Ring",
                "Polygon",
                "MultiPolygon",
                "Geometry",
                "Tuple(Float64,Float64)",
                "Array(Array(Tuple(Float64,Float64)))",
            ] {
                let result = compare_types(&pg(name, -1), target);
                assert_eq!(result.status, Status::Mismatch);
                assert_eq!(result.code, "geo_representation_mismatch");
            }
        }
        let mut source = pg("geometry", -1);
        source.schema = "spatial".into();
        source.extension = Some("postgis".into());
        source.geo = Some(GeoMetadata {
            family: "geometry".into(),
            subtype: Some("PointZM".into()),
            srid: Some(4326),
            dimensions: Some(4),
            has_z: Some(true),
            has_m: Some(true),
        });
        assert_eq!(compare_types(&source, "String").status, Status::Match);
        assert_eq!(
            compare_types(&source, "LowCardinality(String)").status,
            Status::Compatible
        );
        source.extension = None;
        assert_eq!(compare_types(&source, "String").status, Status::Unknown);
    }

    #[test]
    fn parser_handles_whitespace_and_quoted_timezone_without_splitting_on_commas() {
        let parsed = parse_target(" Nullable ( DateTime64 ( 6 , 'A,B\\\'C' ) ) ")
            .expect("valid quoted timezone");
        assert!(parsed.nullable);
        assert_eq!(
            parsed.scalar,
            Scalar::DateTime64 {
                precision: 6,
                timezone: Some("A,B'C".into())
            }
        );
        let parsed = parse_target("DateTime64(6, 'A''B')").expect("doubled quote");
        assert_eq!(
            parsed.scalar,
            Scalar::DateTime64 {
                precision: 6,
                timezone: Some("A'B".into())
            }
        );
    }

    #[test]
    fn malformed_or_unbounded_types_are_unknown_without_panicking() {
        let mut cases = vec![
            "".to_owned(),
            "String garbage".into(),
            "Nullable()".into(),
            "Nullable(Int32".into(),
            "Nullable(Nullable(String))".into(),
            "Array(Int32)".into(),
            "Tuple(String,Int32)".into(),
            "Decimal(0,0)".into(),
            "Decimal(77,2)".into(),
            "Decimal32(10)".into(),
            "Decimal(18,-1)".into(),
            "DateTime64(10)".into(),
            "DateTime64(6,'UTC)".into(),
            "DateTime64(6,'UTC\\')".into(),
            "DateTime64(6,'U\\nTC')".into(),
            "DateTime64(6,UTC)".into(),
            "FixedString(0)".into(),
            "Enum8('x'=1)".into(),
            "🦀".into(),
        ];
        cases.push(format!(
            "{}String{}",
            "Nullable(".repeat(100),
            ")".repeat(100)
        ));
        cases.push("x".repeat(4097));
        cases.extend([
            "Nullable(LowCardinality(String))".into(),
            "Nullable(Array(Tuple(Float64,Float64)))".into(),
            "Nullable(Polygon)".into(),
            "Array(Geometry)".into(),
        ]);
        for case in cases {
            assert!(parse_target(&case).is_err(), "{case}");
        }
    }
}
