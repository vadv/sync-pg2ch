//! SQL rendering and conservative recognition of dependency-bearing definitions.
use super::Operation;
use crate::config::TargetTable;

/// Quote one literal ClickHouse identifier, including backslashes and controls.
pub(crate) fn quote_identifier(name: &str) -> String {
    let mut result = String::from("\"");
    for character in name.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            _ => result.push(character),
        }
    }
    result.push('"');
    result
}

pub(super) fn statement(target: &TargetTable, operations: &[Operation]) -> String {
    let mut clauses = Vec::new();
    for operation in operations {
        let action = match operation {
            Operation::AddColumn { .. } => "ADD",
            Operation::ModifyColumn { .. } => "MODIFY",
        };
        clauses.push(format!(
            "{action} COLUMN {} {}",
            quote_identifier(operation.column()),
            operation.data_type()
        ));
    }
    format!(
        "ALTER TABLE {}.{}\n    {};",
        quote_identifier(&target.database),
        quote_identifier(&target.table),
        clauses.join(",\n    ")
    )
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Identifier(String),
    Literal,
    Symbol(char),
}

fn tokens(input: &str) -> Option<Vec<Token>> {
    let mut chars = input.chars().peekable();
    let mut result = Vec::new();
    while let Some(character) = chars.next() {
        if character.is_whitespace() {
            continue;
        }
        if character == '\'' || character == '"' || character == '\u{60}' {
            let mut value = String::new();
            loop {
                let next = chars.next()?;
                if next == '\\' {
                    let escaped = chars.next()?;
                    value.push(match escaped {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '0' => '\0',
                        _ => escaped,
                    });
                } else if next == character {
                    if chars.peek() == Some(&character) {
                        chars.next();
                        value.push(character);
                    } else {
                        break;
                    }
                } else {
                    value.push(next);
                }
            }
            result.push(if character == '\'' {
                Token::Literal
            } else {
                Token::Identifier(value)
            });
        } else if character.is_alphabetic() || character == '_' {
            let mut word = character.to_string();
            while chars
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            {
                word.push(chars.next()?);
            }
            result.push(Token::Word(word));
        } else if character.is_ascii_digit() {
            while chars
                .peek()
                .is_some_and(|c| c.is_ascii_digit() || *c == '.')
            {
                chars.next();
            }
            result.push(Token::Literal);
        } else {
            result.push(Token::Symbol(character));
        }
    }
    Some(result)
}

pub(super) fn constant_expression(expression: &str) -> bool {
    let Some(tokens) = tokens(expression) else {
        return false;
    };
    for token in &tokens {
        match token {
            Token::Literal | Token::Symbol('+' | '-' | '.') => {}
            _ => return false,
        }
    }
    !tokens.is_empty()
}

pub(super) fn generic_codec(codec: &str) -> bool {
    let Some(tokens) = tokens(codec) else {
        return false;
    };
    for token in tokens {
        match token {
            Token::Word(word)
                if ["CODEC", "LZ4", "LZ4HC", "ZSTD", "NONE"].contains(&word.as_str()) => {}
            Token::Literal | Token::Symbol('(' | ')' | ',') => {}
            _ => return false,
        }
    }
    true
}

pub(super) fn complex_definition(definition: &str) -> bool {
    let Some(tokens) = tokens(definition) else {
        return true;
    };
    for token in tokens {
        if let Token::Word(word) = token
            && ["TTL", "CONSTRAINT"].contains(&word.as_str())
        {
            return true;
        }
    }
    false
}

pub(super) fn engine_columns(definition: &str) -> Option<Vec<String>> {
    let tokens = tokens(definition)?;
    let Token::Word(engine) = tokens.first()? else {
        return None;
    };
    if !["MergeTree", "ReplacingMergeTree"].contains(&engine.as_str()) {
        return None;
    }
    if tokens.get(1) != Some(&Token::Symbol('(')) {
        return Some(Vec::new());
    }
    let mut columns = Vec::new();
    let mut index = 2;
    if tokens.get(index) == Some(&Token::Symbol(')')) {
        return Some(columns);
    }
    loop {
        match tokens.get(index)? {
            Token::Word(name) | Token::Identifier(name) => columns.push(name.clone()),
            _ => return None,
        }
        index += 1;
        match tokens.get(index)? {
            Token::Symbol(')') => return Some(columns),
            Token::Symbol(',') => index += 1,
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_and_definition_literals_cannot_become_sql() {
        assert_eq!(quote_identifier("a\"\\\nЮ"), "\"a\\\"\\\\\\nЮ\"");
        assert!(!complex_definition(
            "CREATE TABLE t (x String DEFAULT 'TTL', \"CONSTRAINT\" Int32) ENGINE=MergeTree ORDER BY x"
        ));
        assert!(complex_definition(
            "CREATE TABLE t (x Date) ENGINE=MergeTree ORDER BY x TTL x"
        ));
        assert_eq!(
            engine_columns("ReplacingMergeTree(\"v.er\", \"is_deleted\") ORDER BY x"),
            Some(vec!["v.er".into(), "is_deleted".into()])
        );
        assert_eq!(
            engine_columns("ReplacingMergeTree(toUInt64(x)) ORDER BY x"),
            None
        );
        assert!(constant_expression("'a\\'b'"));
        assert!(!constant_expression("other_column + 1"));
    }
}
