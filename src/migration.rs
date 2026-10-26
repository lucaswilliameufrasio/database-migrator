use std::path::Path;

use serde::Serialize;

use crate::error::{DbmError, Result};
use crate::identity::{MigrationIdentity, pair_checksum, raw_checksum, repeatable_migration_key};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationFormat {
    Single,
    Pair,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationKind {
    Versioned,
    Repeatable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlDialect {
    Sqlite,
    Postgres,
    Mysql,
}

impl SqlDialect {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransactionPolicy {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, Serialize)]
pub struct Migration {
    pub identity: MigrationIdentity,
    pub kind: MigrationKind,
    pub description: String,
    pub filename: String,
    pub source_path: String,
    pub checksum: String,
    pub up_sql: String,
    pub down_sql: Option<String>,
    pub dialect: SqlDialect,
    pub format: MigrationFormat,
    pub transaction: TransactionPolicy,
}

impl Migration {
    pub fn migration_key(&self) -> String {
        match self.kind {
            MigrationKind::Versioned => crate::identity::migration_key(&self.identity),
            MigrationKind::Repeatable => repeatable_migration_key(&self.identity),
        }
    }
}

pub fn split_sql_statements_for_dialect(sql: &str, dialect: SqlDialect) -> Vec<&str> {
    match dialect {
        SqlDialect::Sqlite => split_sqlite_statements(sql),
        SqlDialect::Postgres => split_postgres_statements(sql),
        SqlDialect::Mysql => split_mysql_statements(sql),
    }
}

pub(crate) fn validate_mysql_migration_statement(sql: &str) -> Result<()> {
    let tokens = mysql_identifier_tokens(sql);
    let Some(first) = tokens.first().map(|token| token.value.as_str()) else {
        return Err(DbmError::Safety(
            "MySQL migration step contains no SQL command".to_owned(),
        ));
    };
    if matches!(
        first,
        "BEGIN"
            | "START"
            | "COMMIT"
            | "END"
            | "ROLLBACK"
            | "SAVEPOINT"
            | "RELEASE"
            | "XA"
            | "USE"
            | "SET"
            | "RESET"
            | "LOCK"
            | "UNLOCK"
            | "DELIMITER"
            | "CALL"
            | "DO"
            | "PREPARE"
            | "EXECUTE"
            | "DEALLOCATE"
            | "HANDLER"
            | "LOAD"
            | "SHUTDOWN"
            | "KILL"
            | "FLUSH"
            | "INSTALL"
            | "UNINSTALL"
            | "GRANT"
            | "REVOKE"
    ) {
        return Err(DbmError::Safety(format!(
            "MySQL migration statement `{first}` is controlled by the runner or outside the supported schema scope"
        )));
    }
    if tokens.iter().any(|token| {
        matches!(
            token.value.as_str(),
            "SCHEMA_MIGRATION_META"
                | "SCHEMA_MIGRATIONS"
                | "SCHEMA_MIGRATION_RUNS"
                | "SCHEMA_MIGRATION_STEPS"
                | "SCHEMA_MIGRATION_LOCKS"
        )
    }) {
        return Err(DbmError::Safety(
            "MySQL migrations cannot access Database Migrator metadata tables".to_owned(),
        ));
    }
    if let Some(object_type) = mysql_created_executable_object(&tokens) {
        return Err(DbmError::Safety(format!(
            "MySQL migrations cannot create stored/executable object type `{object_type}`"
        )));
    }
    if matches!(first, "CREATE" | "ALTER" | "DROP")
        && tokens.get(1).is_some_and(|token| {
            matches!(
                token.value.as_str(),
                "DATABASE" | "SCHEMA" | "USER" | "ROLE" | "TABLESPACE" | "SERVER"
            )
        })
    {
        return Err(DbmError::Safety(
            "MySQL migrations cannot alter server-wide objects or variables".to_owned(),
        ));
    }
    if has_mysql_executable_comment(sql) {
        return Err(DbmError::Safety(
            "MySQL executable version comments are not supported in migrations".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_postgres_migration_statement(sql: &str) -> Result<()> {
    let tokens = postgres_identifier_tokens(sql);
    let Some(first) = tokens.first().map(|token| token.value.as_str()) else {
        return Err(DbmError::Safety(
            "PostgreSQL migration step contains no SQL command".to_owned(),
        ));
    };

    if matches!(
        first,
        "BEGIN"
            | "START"
            | "COMMIT"
            | "END"
            | "ROLLBACK"
            | "ABORT"
            | "SAVEPOINT"
            | "RELEASE"
            | "PREPARE"
            | "DEALLOCATE"
            | "DISCARD"
            | "SET"
            | "RESET"
            | "VACUUM"
            | "CLUSTER"
            | "DO"
            | "CALL"
    ) {
        return Err(DbmError::Safety(format!(
            "PostgreSQL migration statement `{first}` is managed by the runner or is not supported transactionally"
        )));
    }

    if tokens.iter().any(|token| {
        matches!(
            token.value.as_str(),
            "SCHEMA_MIGRATION_META"
                | "SCHEMA_MIGRATIONS"
                | "SCHEMA_MIGRATION_RUNS"
                | "SCHEMA_MIGRATION_STEPS"
                | "SCHEMA_MIGRATION_LOCKS"
        )
    }) {
        return Err(DbmError::Safety(
            "PostgreSQL migrations cannot access Database Migrator metadata tables".to_owned(),
        ));
    }

    if tokens.first().is_some_and(|token| token.value == "ALTER")
        && tokens.get(1).is_some_and(|token| {
            matches!(
                token.value.as_str(),
                "SYSTEM" | "DATABASE" | "ROLE" | "USER" | "TABLESPACE"
            )
        })
        || tokens.first().is_some_and(|token| token.value == "CREATE")
            && tokens.get(1).is_some_and(|token| {
                matches!(
                    token.value.as_str(),
                    "DATABASE" | "ROLE" | "USER" | "TABLESPACE"
                )
            })
        || tokens.first().is_some_and(|token| token.value == "DROP")
            && tokens.get(1).is_some_and(|token| {
                matches!(
                    token.value.as_str(),
                    "DATABASE" | "ROLE" | "USER" | "TABLESPACE"
                )
            })
    {
        return Err(DbmError::Safety(
            "PostgreSQL migration statement changes server-wide objects outside the managed schema"
                .to_owned(),
        ));
    }

    Ok(())
}

pub(crate) fn validate_postgres_nontransactional_statement(sql: &str) -> Result<()> {
    parse_postgres_nontransactional_index_operation(sql).map(|_| ())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PostgresNonTransactionalIndexOperation {
    Create {
        index_name: String,
        table_name: String,
        unique: bool,
        columns: Vec<String>,
    },
    Drop {
        index_name: String,
    },
}

pub(crate) fn parse_postgres_nontransactional_index_operation(
    sql: &str,
) -> Result<PostgresNonTransactionalIndexOperation> {
    validate_postgres_migration_statement(sql)?;
    let tokens = postgres_identifier_tokens(sql);
    let token_values: Vec<&str> = tokens.iter().map(|token| token.value.as_str()).collect();
    let unsupported = || {
        DbmError::Safety(
            "PostgreSQL non-transactional migrations currently support only one unquoted, unqualified CREATE [UNIQUE] INDEX CONCURRENTLY or DROP INDEX CONCURRENTLY statement"
                .to_owned(),
        )
    };

    let first = token_values.first().copied().ok_or_else(unsupported)?;
    match first {
        "CREATE" => {
            let mut index_position = 1;
            let unique = token_values.get(index_position).copied() == Some("UNIQUE");
            if unique {
                index_position += 1;
            }
            if token_values.get(index_position).copied() != Some("INDEX")
                || token_values.get(index_position + 1).copied() != Some("CONCURRENTLY")
            {
                return Err(unsupported());
            }
            let mut name_position = index_position + 2;
            if token_values.get(name_position).copied() == Some("IF") {
                if token_values.get(name_position + 1).copied() != Some("NOT")
                    || token_values.get(name_position + 2).copied() != Some("EXISTS")
                {
                    return Err(unsupported());
                }
                name_position += 3;
            }
            let index_token = tokens.get(name_position).ok_or_else(unsupported)?;
            if index_token.quoted
                || has_dot_after_identifier(sql, index_token)
                || token_values.get(name_position + 1).copied() != Some("ON")
            {
                return Err(unsupported());
            }
            let table_position = name_position + 2;
            let table_token = tokens.get(table_position).ok_or_else(unsupported)?;
            if table_token.quoted || has_dot_after_identifier(sql, table_token) {
                return Err(unsupported());
            }
            let index_name = normalize_unquoted_identifier(&index_token.value)?;
            let table_name = normalize_unquoted_identifier(&table_token.value)?;
            let columns_start = next_nontrivia_position(sql.as_bytes(), table_token.end);
            let (columns, statement_end) = parse_unquoted_column_list(sql, columns_start)?;
            ensure_statement_terminator(sql, statement_end)?;
            Ok(PostgresNonTransactionalIndexOperation::Create {
                index_name,
                table_name,
                unique,
                columns,
            })
        }
        "DROP" => {
            if token_values.get(1).copied() != Some("INDEX")
                || token_values.get(2).copied() != Some("CONCURRENTLY")
            {
                return Err(unsupported());
            }
            let mut name_position = 3;
            if token_values.get(name_position).copied() == Some("IF") {
                if token_values.get(name_position + 1).copied() != Some("EXISTS") {
                    return Err(unsupported());
                }
                name_position += 2;
            }
            let index_token = tokens.get(name_position).ok_or_else(unsupported)?;
            if index_token.quoted || has_dot_after_identifier(sql, index_token) {
                return Err(unsupported());
            }
            let mut trailing = name_position + 1;
            if token_values
                .get(trailing)
                .is_some_and(|token| matches!(*token, "CASCADE" | "RESTRICT"))
            {
                trailing += 1;
            }
            if trailing != token_values.len() {
                return Err(unsupported());
            }
            let statement_end = if trailing > name_position + 1 {
                tokens[trailing - 1].end
            } else {
                index_token.end
            };
            ensure_statement_terminator(sql, statement_end)?;
            Ok(PostgresNonTransactionalIndexOperation::Drop {
                index_name: normalize_unquoted_identifier(&index_token.value)?,
            })
        }
        _ => Err(unsupported()),
    }
}

struct PostgresIdentifierToken {
    value: String,
    quoted: bool,
    end: usize,
}

fn postgres_identifier_tokens(sql: &str) -> Vec<PostgresIdentifierToken> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut lexer = PostgresLexer::default();
    let mut index = 0;

    while index < bytes.len() {
        if !lexer.is_code() {
            lexer.advance(bytes, &mut index);
            continue;
        }

        if bytes[index] == b'"' {
            index += 1;
            let mut identifier = Vec::new();
            while index < bytes.len() {
                if bytes[index] == b'"' {
                    if bytes.get(index + 1) == Some(&b'"') {
                        identifier.push(b'"');
                        index += 2;
                    } else {
                        index += 1;
                        break;
                    }
                } else {
                    identifier.push(bytes[index]);
                    index += 1;
                }
            }
            tokens.push(PostgresIdentifierToken {
                value: String::from_utf8_lossy(&identifier).to_ascii_uppercase(),
                quoted: true,
                end: index,
            });
            continue;
        }

        if is_postgres_identifier_start(bytes[index]) {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (is_postgres_identifier_byte(bytes[index]) || bytes[index] == b'$')
            {
                index += 1;
            }
            tokens.push(PostgresIdentifierToken {
                value: String::from_utf8_lossy(&bytes[start..index]).to_ascii_uppercase(),
                quoted: false,
                end: index,
            });
            continue;
        }

        lexer.advance(bytes, &mut index);
    }

    tokens
}

fn has_dot_after_identifier(sql: &str, token: &PostgresIdentifierToken) -> bool {
    let bytes = sql.as_bytes();
    let index = next_nontrivia_position(bytes, token.end);
    bytes.get(index) == Some(&b'.')
}

fn next_nontrivia_position(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes.get(index) == Some(&b'-') && bytes.get(index + 1) == Some(&b'-') {
            index += 2;
            while bytes.get(index).is_some_and(|byte| *byte != b'\n') {
                index += 1;
            }
            continue;
        }
        if bytes.get(index) == Some(&b'/') && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            let mut depth = 1usize;
            while index < bytes.len() && depth > 0 {
                if bytes.get(index) == Some(&b'/') && bytes.get(index + 1) == Some(&b'*') {
                    depth += 1;
                    index += 2;
                } else if bytes.get(index) == Some(&b'*') && bytes.get(index + 1) == Some(&b'/') {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            continue;
        }
        return index;
    }
}

fn normalize_unquoted_identifier(identifier: &str) -> Result<String> {
    if identifier.is_empty()
        || !identifier.is_ascii()
        || identifier.len() > 63
        || !identifier
            .bytes()
            .all(|byte| is_postgres_identifier_byte(byte) || byte == b'$')
    {
        return Err(DbmError::Safety(
            "PostgreSQL reconciliation supports unquoted ASCII identifiers up to 63 bytes"
                .to_owned(),
        ));
    }
    Ok(identifier.to_ascii_lowercase())
}

fn parse_unquoted_column_list(sql: &str, mut position: usize) -> Result<(Vec<String>, usize)> {
    let bytes = sql.as_bytes();
    if bytes.get(position) != Some(&b'(') {
        return Err(DbmError::Safety(
            "PostgreSQL concurrent index reconciliation supports only a simple column list"
                .to_owned(),
        ));
    }
    position += 1;
    let mut columns = Vec::new();
    loop {
        position = next_nontrivia_position(bytes, position);
        let Some((column, end)) = parse_unquoted_identifier_at(sql, position) else {
            return Err(DbmError::Safety(
                "PostgreSQL concurrent index reconciliation supports only unquoted column names"
                    .to_owned(),
            ));
        };
        columns.push(normalize_unquoted_identifier(column)?);
        position = next_nontrivia_position(bytes, end);
        match bytes.get(position) {
            Some(b',') => {
                position += 1;
            }
            Some(b')') if !columns.is_empty() => {
                return Ok((columns, position + 1));
            }
            _ => {
                return Err(DbmError::Safety(
                    "PostgreSQL concurrent index reconciliation supports only a simple column list"
                        .to_owned(),
                ));
            }
        }
    }
}

fn parse_unquoted_identifier_at(sql: &str, position: usize) -> Option<(&str, usize)> {
    let bytes = sql.as_bytes();
    if !bytes
        .get(position)
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    {
        return None;
    }
    let mut end = position + 1;
    while bytes
        .get(end)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'$')
    {
        end += 1;
    }
    Some((&sql[position..end], end))
}

fn ensure_statement_terminator(sql: &str, mut position: usize) -> Result<()> {
    let bytes = sql.as_bytes();
    position = next_nontrivia_position(bytes, position);
    if bytes.get(position) == Some(&b';') {
        position += 1;
        position = next_nontrivia_position(bytes, position);
    }
    if position != bytes.len() {
        return Err(DbmError::Safety(
            "PostgreSQL concurrent-index reconciliation rejects clauses outside the supported simple-index grammar"
                .to_owned(),
        ));
    }
    Ok(())
}

fn split_sqlite_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut lex_state = LexState::Code;
    let mut token = Vec::new();
    let mut first_words = Vec::<String>::new();
    let mut is_trigger = false;
    let mut trigger_body = false;
    let mut case_depth = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        match lex_state {
            LexState::Code => {
                if is_word_byte(byte) {
                    token.push(byte.to_ascii_uppercase());
                    index += 1;
                    continue;
                }
                observe_sql_token(
                    &mut token,
                    &mut first_words,
                    &mut is_trigger,
                    &mut trigger_body,
                    &mut case_depth,
                );
                match byte {
                    b'-' if bytes.get(index + 1) == Some(&b'-') => {
                        lex_state = LexState::LineComment;
                        index += 2;
                    }
                    b'/' if bytes.get(index + 1) == Some(&b'*') => {
                        lex_state = LexState::BlockComment;
                        index += 2;
                    }
                    b'\'' => {
                        lex_state = LexState::SingleQuote;
                        index += 1;
                    }
                    b'"' => {
                        lex_state = LexState::DoubleQuote;
                        index += 1;
                    }
                    b'`' => {
                        lex_state = LexState::Backtick;
                        index += 1;
                    }
                    b'[' => {
                        lex_state = LexState::Bracket;
                        index += 1;
                    }
                    b';' if !trigger_body => {
                        let end = index + 1;
                        let statement = &sql[start..end];
                        if has_statement_content(statement) {
                            statements.push(statement);
                        }
                        start = end;
                        index = end;
                        first_words.clear();
                        is_trigger = false;
                        trigger_body = false;
                        case_depth = 0;
                    }
                    _ => index += 1,
                }
            }
            LexState::SingleQuote => {
                if byte == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                    } else {
                        lex_state = LexState::Code;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            LexState::DoubleQuote => {
                if byte == b'"' {
                    if bytes.get(index + 1) == Some(&b'"') {
                        index += 2;
                    } else {
                        lex_state = LexState::Code;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            LexState::Backtick => {
                if byte == b'`' {
                    if bytes.get(index + 1) == Some(&b'`') {
                        index += 2;
                    } else {
                        lex_state = LexState::Code;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            LexState::Bracket => {
                if byte == b']' {
                    lex_state = LexState::Code;
                }
                index += 1;
            }
            LexState::LineComment => {
                if byte == b'\n' {
                    lex_state = LexState::Code;
                }
                index += 1;
            }
            LexState::BlockComment => {
                if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    lex_state = LexState::Code;
                    index += 2;
                } else {
                    index += 1;
                }
            }
        }
    }

    observe_sql_token(
        &mut token,
        &mut first_words,
        &mut is_trigger,
        &mut trigger_body,
        &mut case_depth,
    );
    let trailing = &sql[start..];
    if has_statement_content(trailing) {
        statements.push(trailing);
    }
    statements
}

fn split_postgres_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut lexer = PostgresLexer::default();

    while index < bytes.len() {
        if lexer.is_code() && bytes[index] == b';' {
            let end = index + 1;
            let statement = &sql[start..end];
            if has_statement_content(statement) {
                statements.push(statement);
            }
            start = end;
            index = end;
            continue;
        }
        lexer.advance(bytes, &mut index);
    }

    let trailing = &sql[start..];
    if has_statement_content(trailing) {
        statements.push(trailing);
    }
    statements
}

fn split_mysql_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut lexer = MysqlLexer::default();
    while index < bytes.len() {
        if lexer.is_code() && bytes[index] == b';' {
            let end = index + 1;
            let statement = &sql[start..end];
            if has_statement_content(statement) {
                statements.push(statement);
            }
            start = end;
            index = end;
            continue;
        }
        lexer.advance(bytes, &mut index);
    }
    let trailing = &sql[start..];
    if has_statement_content(trailing) {
        statements.push(trailing);
    }
    statements
}

fn mysql_identifier_tokens(sql: &str) -> Vec<PostgresIdentifierToken> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut lexer = MysqlLexer::default();
    let mut index = 0;
    while index < bytes.len() {
        if !lexer.is_code() {
            lexer.advance(bytes, &mut index);
            continue;
        }
        if matches!(bytes[index], b'`' | b'"') {
            let quote = bytes[index];
            index += 1;
            let mut identifier = Vec::new();
            while index < bytes.len() {
                if bytes[index] == quote {
                    if bytes.get(index + 1) == Some(&quote) {
                        identifier.push(quote);
                        index += 2;
                    } else {
                        index += 1;
                        break;
                    }
                } else if bytes[index] == b'\\' && quote == b'"' {
                    index = (index + 2).min(bytes.len());
                } else {
                    identifier.push(bytes[index]);
                    index += 1;
                }
            }
            tokens.push(PostgresIdentifierToken {
                value: String::from_utf8_lossy(&identifier).to_ascii_uppercase(),
                quoted: true,
                end: index,
            });
            continue;
        }
        if is_postgres_identifier_start(bytes[index]) {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (is_postgres_identifier_byte(bytes[index]) || bytes[index] == b'$')
            {
                index += 1;
            }
            tokens.push(PostgresIdentifierToken {
                value: String::from_utf8_lossy(&bytes[start..index]).to_ascii_uppercase(),
                quoted: false,
                end: index,
            });
            continue;
        }
        lexer.advance(bytes, &mut index);
    }
    tokens
}

fn mysql_created_executable_object(tokens: &[PostgresIdentifierToken]) -> Option<&str> {
    let first = tokens.first()?.value.as_str();
    if first != "CREATE" {
        return None;
    }

    let mut type_index = 1;
    if tokens
        .get(type_index)
        .is_some_and(|token| token.value == "OR")
        && tokens
            .get(type_index + 1)
            .is_some_and(|token| token.value == "REPLACE")
    {
        type_index += 2;
    }
    if tokens
        .get(type_index)
        .is_some_and(|token| matches!(token.value.as_str(), "TEMP" | "TEMPORARY"))
    {
        type_index += 1;
    }

    let is_executable_type = |value: &str| {
        matches!(
            value,
            "PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT" | "PLUGIN" | "COMPONENT"
        )
    };
    let type_token = tokens.get(type_index)?;
    if type_token.value == "DEFINER" {
        return tokens
            .iter()
            .skip(type_index + 1)
            .take(8)
            .find(|token| is_executable_type(&token.value))
            .map(|token| token.value.as_str());
    }
    is_executable_type(&type_token.value).then_some(type_token.value.as_str())
}

fn has_mysql_executable_comment(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut lexer = MysqlLexer::default();
    let mut index = 0;
    while index < bytes.len() {
        if lexer.is_code() && bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            let next = bytes.get(index + 2).copied();
            let maria_marker = bytes
                .get(index + 2..index + 4)
                .is_some_and(|marker| marker.eq_ignore_ascii_case(b"m!"));
            if next == Some(b'!') || maria_marker {
                return true;
            }
        }
        lexer.advance(bytes, &mut index);
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn observe_sql_token(
    token: &mut Vec<u8>,
    first_words: &mut Vec<String>,
    is_trigger: &mut bool,
    trigger_body: &mut bool,
    case_depth: &mut usize,
) {
    if token.is_empty() {
        return;
    }
    let word = String::from_utf8(std::mem::take(token)).unwrap_or_default();

    if *trigger_body {
        match word.as_str() {
            "CASE" => *case_depth += 1,
            "END" if *case_depth > 0 => *case_depth -= 1,
            "END" => *trigger_body = false,
            _ => {}
        }
        return;
    }

    if *is_trigger {
        if word == "BEGIN" {
            *trigger_body = true;
        }
        return;
    }

    if first_words.len() < 3 {
        first_words.push(word.clone());
    }
    if first_words.as_slice() == ["CREATE", "TRIGGER"]
        || first_words.as_slice() == ["CREATE", "TEMP", "TRIGGER"]
        || first_words.as_slice() == ["CREATE", "TEMPORARY", "TRIGGER"]
    {
        *is_trigger = true;
    }
}

pub fn parse_single(
    identity: MigrationIdentity,
    description: String,
    path: &Path,
    source_path: String,
    bytes: &[u8],
) -> Result<Migration> {
    parse_single_internal(identity, description, path, source_path, bytes, false)
}

pub fn parse_repeatable(
    identity: MigrationIdentity,
    description: String,
    path: &Path,
    source_path: String,
    bytes: &[u8],
) -> Result<Migration> {
    let mut migration =
        parse_single_internal(identity, description, path, source_path, bytes, true)?;
    if migration.down_sql.is_some() {
        return Err(invalid(
            path,
            "repeatable migrations cannot declare a down section",
        ));
    }
    migration.kind = MigrationKind::Repeatable;
    Ok(migration)
}

fn parse_single_internal(
    identity: MigrationIdentity,
    description: String,
    path: &Path,
    source_path: String,
    bytes: &[u8],
    allow_body_without_up_marker: bool,
) -> Result<Migration> {
    let filename = file_name(path)?;
    let contents = std::str::from_utf8(bytes).map_err(|error| invalid(path, error.to_string()))?;
    let mut section = Section::Preamble;
    let mut lex_state = LexState::Code;
    let mut up_sql = String::new();
    let mut down_sql = String::new();
    let mut has_up_marker = false;
    let mut has_down_marker = false;
    let mut dialect = SqlDialect::Sqlite;
    let mut transaction = TransactionPolicy::Auto;
    let mut irreversible = false;
    let mut postgres_lexer = PostgresLexer::default();
    let mut mysql_lexer = MysqlLexer::default();

    for (line_index, segment) in contents.split_inclusive('\n').enumerate() {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let trimmed = line.trim();
        let directive_allowed = match dialect {
            SqlDialect::Sqlite => lex_state == LexState::Code,
            SqlDialect::Postgres => postgres_lexer.is_code(),
            SqlDialect::Mysql => mysql_lexer.is_code(),
        };
        let directive = if directive_allowed {
            trimmed.strip_prefix("-- dbm:")
        } else {
            None
        };

        if let Some(directive) = directive {
            let mut words = directive.split_whitespace();
            let command = words.next().unwrap_or_default();
            let argument = words.next();
            let has_extra = words.next().is_some();

            match command {
                "dialect" if section == Section::Preamble && !has_extra => {
                    dialect = match argument.ok_or_else(|| {
                        invalid(path, format!("line {}: missing dialect", line_index + 1))
                    })? {
                        "sqlite" => SqlDialect::Sqlite,
                        "postgres" => SqlDialect::Postgres,
                        "mysql" => SqlDialect::Mysql,
                        value => {
                            return Err(invalid(
                                path,
                                format!("line {}: unsupported dialect {value}", line_index + 1),
                            ));
                        }
                    };
                }
                "transaction" if section == Section::Preamble && !has_extra => {
                    transaction = match argument {
                        Some("auto") => TransactionPolicy::Auto,
                        Some("always") => TransactionPolicy::Always,
                        Some("never")
                            if matches!(dialect, SqlDialect::Postgres | SqlDialect::Mysql) =>
                        {
                            TransactionPolicy::Never
                        }
                        Some("never") => {
                            return Err(DbmError::Unsupported(format!(
                                "{}: transaction=never requires a driver that supports non-transactional execution",
                                path.display()
                            )));
                        }
                        _ => {
                            return Err(invalid(
                                path,
                                format!(
                                    "line {}: transaction must be auto, always, or a supported explicit never",
                                    line_index + 1
                                ),
                            ));
                        }
                    };
                }
                "irreversible" if section == Section::Preamble && argument.is_none() => {
                    irreversible = true;
                }
                "up" if argument.is_none() && !has_extra => {
                    if has_up_marker || has_down_marker {
                        return Err(invalid(
                            path,
                            format!(
                                "line {}: duplicate or out-of-order up marker",
                                line_index + 1
                            ),
                        ));
                    }
                    has_up_marker = true;
                    section = Section::Up;
                }
                "down" if argument.is_none() && !has_extra => {
                    if !has_up_marker || has_down_marker {
                        return Err(invalid(
                            path,
                            format!(
                                "line {}: down marker requires one preceding up marker",
                                line_index + 1
                            ),
                        ));
                    }
                    has_down_marker = true;
                    section = Section::Down;
                }
                _ => {
                    return Err(invalid(
                        path,
                        format!(
                            "line {}: unsupported or misplaced directive -- dbm:{directive}",
                            line_index + 1
                        ),
                    ));
                }
            }
            continue;
        }

        if section == Section::Preamble {
            if !trimmed.is_empty() && !trimmed.starts_with("--") {
                if allow_body_without_up_marker {
                    has_up_marker = true;
                    section = Section::Up;
                    up_sql.push_str(segment);
                } else {
                    return Err(invalid(
                        path,
                        format!("line {}: SQL content must follow -- dbm:up", line_index + 1),
                    ));
                }
            }
        } else {
            match section {
                Section::Up => up_sql.push_str(segment),
                Section::Down => down_sql.push_str(segment),
                Section::Preamble => {}
            }
        }

        match dialect {
            SqlDialect::Sqlite => advance_lex_state(segment, &mut lex_state),
            SqlDialect::Postgres => {
                let bytes = segment.as_bytes();
                let mut index = 0;
                while index < bytes.len() {
                    postgres_lexer.advance(bytes, &mut index);
                }
            }
            SqlDialect::Mysql => {
                let bytes = segment.as_bytes();
                let mut index = 0;
                while index < bytes.len() {
                    mysql_lexer.advance(bytes, &mut index);
                }
            }
        }
    }

    if !has_up_marker {
        return Err(invalid(path, "missing -- dbm:up section"));
    }
    if !has_statement_content(&up_sql) {
        return Err(invalid(
            path,
            "up section is empty or contains comments only",
        ));
    }
    let down_sql = if has_down_marker {
        if irreversible {
            return Err(invalid(
                path,
                "an irreversible migration cannot also declare a down section",
            ));
        }
        if !has_statement_content(&down_sql) {
            return Err(invalid(
                path,
                "down section is empty or contains comments only",
            ));
        }
        Some(down_sql)
    } else {
        None
    };

    Ok(Migration {
        identity,
        kind: MigrationKind::Versioned,
        description,
        filename,
        source_path,
        checksum: raw_checksum(bytes),
        up_sql,
        down_sql,
        dialect,
        format: MigrationFormat::Single,
        transaction,
    })
}

pub struct PairMigrationInput<'a> {
    pub identity: MigrationIdentity,
    pub description: String,
    pub dialect: SqlDialect,
    pub up_path: &'a Path,
    pub down_path: Option<&'a Path>,
    pub source_path: String,
    pub up_bytes: &'a [u8],
    pub down_bytes: Option<&'a [u8]>,
}

pub fn parse_pair(input: PairMigrationInput<'_>) -> Result<Migration> {
    let PairMigrationInput {
        identity,
        description,
        dialect,
        up_path,
        down_path,
        source_path,
        up_bytes,
        down_bytes,
    } = input;
    let up_sql =
        std::str::from_utf8(up_bytes).map_err(|error| invalid(up_path, error.to_string()))?;
    if !has_statement_content(up_sql) {
        return Err(invalid(
            up_path,
            "up file is empty or contains comments only",
        ));
    }
    reject_pair_directives(up_path, up_sql)?;

    let down_sql = match (down_path, down_bytes) {
        (Some(path), Some(bytes)) => {
            let sql =
                std::str::from_utf8(bytes).map_err(|error| invalid(path, error.to_string()))?;
            if !has_statement_content(sql) {
                return Err(invalid(
                    path,
                    "down file is empty or contains comments only",
                ));
            }
            reject_pair_directives(path, sql)?;
            Some(sql.to_owned())
        }
        (None, None) => None,
        _ => {
            return Err(DbmError::Execution(
                "internal source error: down path and bytes must be present together".to_owned(),
            ));
        }
    };

    let up_name = file_name(up_path)?;
    let filename = match down_path {
        Some(path) => format!("{up_name}|{}", file_name(path)?),
        None => up_name,
    };

    Ok(Migration {
        identity,
        kind: MigrationKind::Versioned,
        description,
        filename,
        source_path,
        checksum: pair_checksum(up_bytes, down_bytes),
        up_sql: up_sql.to_owned(),
        down_sql,
        dialect,
        format: MigrationFormat::Pair,
        transaction: TransactionPolicy::Auto,
    })
}

fn reject_pair_directives(path: &Path, sql: &str) -> Result<()> {
    let mut state = LexState::Code;
    for (line_index, segment) in sql.split_inclusive('\n').enumerate() {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        if state == LexState::Code && line.trim().starts_with("-- dbm:") {
            return Err(invalid(
                path,
                format!(
                    "line {}: dbm directives are only supported in single-file migrations",
                    line_index + 1
                ),
            ));
        }
        advance_lex_state(segment, &mut state);
    }
    Ok(())
}

fn has_statement_content(sql: &str) -> bool {
    let mut state = LexState::Code;
    let mut chars = sql.chars().peekable();

    while let Some(character) = chars.next() {
        match state {
            LexState::Code => match character {
                '-' if chars.peek() == Some(&'-') => {
                    for comment_character in chars.by_ref() {
                        if comment_character == '\n' {
                            break;
                        }
                    }
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    state = LexState::BlockComment;
                }
                '\'' => return true,
                '"' => return true,
                '`' => return true,
                '[' => return true,
                character if character.is_whitespace() || character == ';' => {}
                _ => return true,
            },
            LexState::BlockComment => {
                if character == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = LexState::Code;
                }
            }
            LexState::LineComment => {
                if character == '\n' {
                    state = LexState::Code;
                }
            }
            LexState::SingleQuote
            | LexState::DoubleQuote
            | LexState::Backtick
            | LexState::Bracket => return true,
        }
    }

    false
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Section {
    Preamble,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LexState {
    #[default]
    Code,
    LineComment,
    SingleQuote,
    DoubleQuote,
    Backtick,
    Bracket,
    BlockComment,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PostgresState {
    #[default]
    Code,
    SingleQuote,
    DoubleQuote,
    LineComment,
    BlockComment,
    DollarQuote,
}

#[derive(Clone, Debug, Default)]
struct PostgresLexer {
    state: PostgresState,
    block_depth: usize,
    dollar_delimiter: Vec<u8>,
    escape_string: bool,
}

impl PostgresLexer {
    fn is_code(&self) -> bool {
        self.state == PostgresState::Code
    }

    fn advance(&mut self, bytes: &[u8], index: &mut usize) {
        let byte = bytes[*index];
        match self.state {
            PostgresState::Code => match byte {
                b'-' if bytes.get(*index + 1) == Some(&b'-') => {
                    self.state = PostgresState::LineComment;
                    *index += 2;
                }
                b'/' if bytes.get(*index + 1) == Some(&b'*') => {
                    self.state = PostgresState::BlockComment;
                    self.block_depth = 1;
                    *index += 2;
                }
                b'\'' => {
                    self.escape_string = is_postgres_escape_string(bytes, *index);
                    self.state = PostgresState::SingleQuote;
                    *index += 1;
                }
                b'"' => {
                    self.state = PostgresState::DoubleQuote;
                    *index += 1;
                }
                b'$' => match postgres_dollar_delimiter(bytes, *index) {
                    Some(delimiter) => {
                        self.dollar_delimiter = delimiter.to_vec();
                        self.state = PostgresState::DollarQuote;
                        *index += delimiter.len();
                    }
                    None => *index += 1,
                },
                _ => *index += 1,
            },
            PostgresState::SingleQuote => {
                if self.escape_string && byte == b'\\' {
                    *index = (*index + 2).min(bytes.len());
                } else if byte == b'\'' {
                    if bytes.get(*index + 1) == Some(&b'\'') {
                        *index += 2;
                    } else {
                        self.state = PostgresState::Code;
                        *index += 1;
                    }
                } else {
                    *index += 1;
                }
            }
            PostgresState::DoubleQuote => {
                if byte == b'"' {
                    if bytes.get(*index + 1) == Some(&b'"') {
                        *index += 2;
                    } else {
                        self.state = PostgresState::Code;
                        *index += 1;
                    }
                } else {
                    *index += 1;
                }
            }
            PostgresState::LineComment => {
                if byte == b'\n' {
                    self.state = PostgresState::Code;
                }
                *index += 1;
            }
            PostgresState::BlockComment => {
                if byte == b'/' && bytes.get(*index + 1) == Some(&b'*') {
                    self.block_depth += 1;
                    *index += 2;
                } else if byte == b'*' && bytes.get(*index + 1) == Some(&b'/') {
                    self.block_depth -= 1;
                    if self.block_depth == 0 {
                        self.state = PostgresState::Code;
                    }
                    *index += 2;
                } else {
                    *index += 1;
                }
            }
            PostgresState::DollarQuote => {
                if bytes[*index..].starts_with(&self.dollar_delimiter) {
                    *index += self.dollar_delimiter.len();
                    self.dollar_delimiter.clear();
                    self.state = PostgresState::Code;
                } else {
                    *index += 1;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MysqlLexer {
    state: LexState,
}

impl MysqlLexer {
    fn is_code(&self) -> bool {
        self.state == LexState::Code
    }

    fn advance(&mut self, bytes: &[u8], index: &mut usize) {
        let byte = bytes[*index];
        match self.state {
            LexState::Code => match byte {
                b'\'' => {
                    self.state = LexState::SingleQuote;
                    *index += 1;
                }
                b'"' => {
                    self.state = LexState::DoubleQuote;
                    *index += 1;
                }
                b'`' => {
                    self.state = LexState::Backtick;
                    *index += 1;
                }
                b'#' => {
                    self.state = LexState::LineComment;
                    *index += 1;
                }
                b'-' if bytes.get(*index + 1) == Some(&b'-')
                    && bytes.get(*index + 2).is_some_and(u8::is_ascii_whitespace) =>
                {
                    self.state = LexState::LineComment;
                    *index += 2;
                }
                b'/' if bytes.get(*index + 1) == Some(&b'*') => {
                    self.state = LexState::BlockComment;
                    *index += 2;
                }
                _ => *index += 1,
            },
            LexState::SingleQuote | LexState::DoubleQuote => {
                let delimiter = if self.state == LexState::SingleQuote {
                    b'\''
                } else {
                    b'"'
                };
                if byte == b'\\' {
                    *index = (*index + 2).min(bytes.len());
                } else if byte == delimiter {
                    if bytes.get(*index + 1) == Some(&delimiter) {
                        *index += 2;
                    } else {
                        self.state = LexState::Code;
                        *index += 1;
                    }
                } else {
                    *index += 1;
                }
            }
            LexState::Backtick => {
                if byte == b'`' {
                    if bytes.get(*index + 1) == Some(&b'`') {
                        *index += 2;
                    } else {
                        self.state = LexState::Code;
                        *index += 1;
                    }
                } else {
                    *index += 1;
                }
            }
            LexState::LineComment => {
                if byte == b'\n' {
                    self.state = LexState::Code;
                }
                *index += 1;
            }
            LexState::BlockComment => {
                if byte == b'*' && bytes.get(*index + 1) == Some(&b'/') {
                    self.state = LexState::Code;
                    *index += 2;
                } else {
                    *index += 1;
                }
            }
            LexState::Bracket => *index += 1,
        }
    }
}

fn is_postgres_escape_string(bytes: &[u8], quote_index: usize) -> bool {
    if quote_index == 0 {
        return false;
    }
    let prefix_index = quote_index - 1;
    if !matches!(bytes[prefix_index], b'e' | b'E') {
        return false;
    }
    prefix_index == 0 || !is_postgres_identifier_byte(bytes[prefix_index - 1])
}

fn postgres_dollar_delimiter(bytes: &[u8], start: usize) -> Option<&[u8]> {
    if start > 0 && is_postgres_identifier_byte(bytes[start - 1]) {
        return None;
    }
    let closing_offset = bytes[start + 1..].iter().position(|byte| *byte == b'$')?;
    let end = start + closing_offset + 2;
    let tag = &bytes[start + 1..end - 1];
    if let Some(first) = tag.first()
        && (!is_postgres_identifier_start(*first)
            || !tag
                .iter()
                .skip(1)
                .all(|byte| is_postgres_identifier_byte(*byte)))
    {
        return None;
    }
    Some(&bytes[start..end])
}

fn is_postgres_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn is_postgres_identifier_byte(byte: u8) -> bool {
    is_postgres_identifier_start(byte) || byte.is_ascii_digit()
}

fn advance_lex_state(segment: &str, state: &mut LexState) {
    let mut chars = segment.chars().peekable();
    while let Some(character) = chars.next() {
        match *state {
            LexState::Code => match character {
                '-' if chars.peek() == Some(&'-') => break,
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    *state = LexState::BlockComment;
                }
                '\'' => *state = LexState::SingleQuote,
                '"' => *state = LexState::DoubleQuote,
                '`' => *state = LexState::Backtick,
                '[' => *state = LexState::Bracket,
                _ => {}
            },
            LexState::SingleQuote => {
                if character == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        *state = LexState::Code;
                    }
                }
            }
            LexState::DoubleQuote => {
                if character == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                    } else {
                        *state = LexState::Code;
                    }
                }
            }
            LexState::Backtick => {
                if character == '`' {
                    if chars.peek() == Some(&'`') {
                        chars.next();
                    } else {
                        *state = LexState::Code;
                    }
                }
            }
            LexState::Bracket => {
                if character == ']' {
                    *state = LexState::Code;
                }
            }
            LexState::BlockComment => {
                if character == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    *state = LexState::Code;
                }
            }
            LexState::LineComment => {
                if character == '\n' {
                    *state = LexState::Code;
                }
            }
        }
    }
}

fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid(path, "filename is not valid UTF-8"))
}

fn invalid(path: &Path, message: impl ToString) -> DbmError {
    DbmError::InvalidMigration {
        path: path.display().to_string(),
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        MigrationFormat, MigrationIdentity, PostgresNonTransactionalIndexOperation, SqlDialect,
        TransactionPolicy, parse_postgres_nontransactional_index_operation, parse_single,
        split_sql_statements_for_dialect, validate_mysql_migration_statement,
        validate_postgres_migration_statement, validate_postgres_nontransactional_statement,
    };

    fn identity() -> MigrationIdentity {
        MigrationIdentity {
            namespace: "default".to_owned(),
            version: "20261021162643981234".to_owned(),
        }
    }

    #[test]
    fn parses_single_file_with_up_and_down() {
        let sql = b"-- dbm:dialect sqlite\n-- dbm:transaction always\n-- dbm:up\nCREATE TABLE users (id INTEGER);\n-- dbm:down\nDROP TABLE users;\n";
        let migration = parse_single(
            identity(),
            "create_users".to_owned(),
            Path::new("migration.sql"),
            "migrations/versioned/migration.sql".to_owned(),
            sql,
        )
        .expect("migration parses");

        assert_eq!(migration.format, MigrationFormat::Single);
        assert_eq!(migration.transaction, TransactionPolicy::Always);
        assert!(migration.up_sql.contains("CREATE TABLE users"));
        assert!(
            migration
                .down_sql
                .expect("down section")
                .contains("DROP TABLE")
        );
    }

    #[test]
    fn does_not_treat_directive_text_inside_multiline_literal_as_a_marker() {
        let sql = b"-- dbm:up\nCREATE TABLE t (value TEXT);\nINSERT INTO t VALUES ('first\n-- dbm:down\nlast');\n";
        let migration = parse_single(
            identity(),
            "literal_marker".to_owned(),
            Path::new("migration.sql"),
            "migrations/versioned/migration.sql".to_owned(),
            sql,
        )
        .expect("literal text is not a directive");

        assert!(migration.down_sql.is_none());
        assert!(migration.up_sql.contains("-- dbm:down"));
    }

    #[test]
    fn rejects_non_transactional_and_wrong_dialect_directives() {
        let non_transactional = b"-- dbm:transaction never\n-- dbm:up\nSELECT 1;\n";
        assert!(
            parse_single(
                identity(),
                "non_transactional".to_owned(),
                Path::new("migration.sql"),
                "migrations/versioned/migration.sql".to_owned(),
                non_transactional,
            )
            .is_err()
        );

        let unsupported_dialect = b"-- dbm:dialect oracle\n-- dbm:up\nSELECT 1;\n";
        assert!(
            parse_single(
                identity(),
                "unsupported_dialect".to_owned(),
                Path::new("migration.sql"),
                "migrations/versioned/migration.sql".to_owned(),
                unsupported_dialect,
            )
            .is_err()
        );
    }

    #[test]
    fn parses_postgres_dialect_and_ignores_directive_text_inside_dollar_quotes() {
        let sql = br#"-- dbm:dialect postgres
-- dbm:transaction never
-- dbm:up
DO $body$
BEGIN
    -- dbm:down
    PERFORM 1;
END;
$body$;
SELECT 2;
"#;
        let migration = parse_single(
            identity(),
            "postgres_block".to_owned(),
            Path::new("migration.sql"),
            "migrations/versioned/migration.sql".to_owned(),
            sql,
        )
        .expect("PostgreSQL migration parses");

        assert_eq!(migration.dialect, SqlDialect::Postgres);
        assert_eq!(migration.transaction, TransactionPolicy::Never);
        assert!(migration.down_sql.is_none());
        assert_eq!(
            split_sql_statements_for_dialect(&migration.up_sql, migration.dialect).len(),
            2
        );
    }

    #[test]
    fn parses_mysql_dialect_and_never_transaction_policy() {
        let sql = b"-- dbm:dialect mysql\n-- dbm:transaction never\n-- dbm:up\nCREATE TABLE users (id BIGINT PRIMARY KEY, email VARCHAR(255));\n-- dbm:down\nDROP TABLE users;\n";
        let migration = parse_single(
            identity(),
            "mysql_users".to_owned(),
            Path::new("migration.sql"),
            "migrations/versioned/migration.sql".to_owned(),
            sql,
        )
        .expect("MySQL migration parses");

        assert_eq!(migration.dialect, SqlDialect::Mysql);
        assert_eq!(migration.transaction, TransactionPolicy::Never);
        assert_eq!(
            split_sql_statements_for_dialect(&migration.up_sql, SqlDialect::Mysql).len(),
            1
        );
    }

    #[test]
    fn mysql_scanner_respects_quotes_and_mysql_comments() {
        let slash = char::from(92);
        let sql = format!(
            "# ignored; line comment\n\
             CREATE TABLE `semi;table` (value VARCHAR(40) DEFAULT 'a;b');\n\
             INSERT INTO `semi;table` VALUES ('escaped{slash}';value'); -- comment;\n\
             /* block; comment */ UPDATE `semi;table` SET value = 'x'';y';\n"
        );
        let statements = split_sql_statements_for_dialect(&sql, SqlDialect::Mysql);

        assert_eq!(statements.len(), 3);
        assert!(statements[0].contains("`semi;table`"));
        assert!(statements[1].contains("escaped"));
        assert!(statements[2].contains("UPDATE"));
    }

    #[test]
    fn mysql_guard_blocks_runner_control_dynamic_sql_and_executable_comments() {
        for sql in [
            "START TRANSACTION",
            "SET autocommit = 0",
            "PREPARE stmt FROM 'DROP TABLE x'",
            "EXECUTE stmt",
            "CALL mutate_schema()",
            "CREATE PROCEDURE mutate_schema() BEGIN SELECT 1; END",
            "CREATE DEFINER = CURRENT_USER PROCEDURE mutate_schema() BEGIN SELECT 1; END",
            "CREATE USER migrator",
            "UPDATE schema_migration_locks SET state = 'released'",
            "/*!80000 DROP TABLE sensitive */",
            "/*M!100100 DROP TABLE sensitive */",
        ] {
            assert!(
                validate_mysql_migration_statement(sql).is_err(),
                "expected MySQL migration guard to reject {sql:?}"
            );
        }
    }

    #[test]
    fn mysql_guard_ignores_reserved_words_inside_literals_and_comments() {
        for sql in [
            "CREATE TABLE app_data (message VARCHAR(100) DEFAULT 'COMMIT schema_migration_runs')",
            "-- SET autocommit\nCREATE TABLE app_data (id INTEGER)",
            "INSERT INTO app_data VALUES ('/*! executable-looking text */')",
            "CREATE TABLE app_data (`event` VARCHAR(20), `procedure` VARCHAR(30))",
        ] {
            assert!(
                validate_mysql_migration_statement(sql).is_ok(),
                "expected comments/literals not to trigger the MySQL guard for {sql:?}"
            );
        }
    }

    #[test]
    fn splits_statements_without_breaking_literals_comments_or_triggers() {
        let sql = "-- leading; comment\n\
                   CREATE TABLE items (value TEXT);\n\
                   INSERT INTO items VALUES ('semi;colon');\n\
                   /* interior; comment */\n\
                   CREATE TRIGGER items_audit AFTER INSERT ON items BEGIN\n\
                       INSERT INTO items VALUES (CASE WHEN NEW.value = 'x' THEN 'a;b' ELSE 'c' END);\n\
                       UPDATE items SET value = value WHERE value = 'semi;colon';\n\
                   END;\n\
                   SELECT 1; -- trailing; comment\n";

        let statements = split_sql_statements_for_dialect(sql, SqlDialect::Sqlite);

        assert_eq!(statements.len(), 4);
        assert!(statements[0].contains("CREATE TABLE"));
        assert!(statements[1].contains("INSERT INTO items"));
        assert!(statements[2].contains("CREATE TRIGGER"));
        assert!(statements[2].contains("UPDATE items"));
        assert!(statements[3].contains("SELECT 1"));
    }

    #[test]
    fn postgres_scanner_handles_dollar_quotes_escape_strings_and_nested_comments() {
        let slash = char::from(92);
        let sql = format!(
            r#"CREATE FUNCTION f() RETURNS void AS $function$
BEGIN
    PERFORM 'semi;colon';
    /* outer comment; /* nested comment; */ still a comment */
END;
$function$ LANGUAGE plpgsql;
SELECT E'escaped{slash}';semicolon';
SELECT $tag$quoted; /* body */$tag$;
"#
        );
        let statements = split_sql_statements_for_dialect(&sql, SqlDialect::Postgres);

        assert_eq!(statements.len(), 3);
        assert!(statements[0].contains("END;\n$function$"));
        assert!(statements[1].contains("E'escaped"));
        assert!(statements[2].contains("$tag$quoted; /* body */$tag$"));
    }

    #[test]
    fn postgres_migration_guard_blocks_runner_control_and_metadata_access() {
        for sql in [
            "/* leading comment */ BEGIN",
            "COMMIT",
            "SET search_path TO public",
            "CREATE TABLE \"schema_migration_runs\" (id integer)",
            "UPDATE public.schema_migration_locks SET state = 'released'",
            "CREATE DATABASE other_db",
            "DO $$ BEGIN NULL; END $$",
        ] {
            assert!(
                validate_postgres_migration_statement(sql).is_err(),
                "expected runner guard to reject {sql:?}"
            );
        }
    }

    #[test]
    fn postgres_migration_guard_ignores_literals_and_comments() {
        for sql in [
            "CREATE TABLE app_data (message text DEFAULT 'COMMIT schema_migration_runs')",
            "-- SET search_path\nCREATE TABLE app_data (id integer)",
            "SELECT '$$ schema_migration_steps $$'",
        ] {
            assert!(
                validate_postgres_migration_statement(sql).is_ok(),
                "expected literals/comments not to trigger the guard for {sql:?}"
            );
        }
    }

    #[test]
    fn postgres_nontransactional_guard_accepts_only_concurrent_index_ddl() {
        for sql in [
            "CREATE INDEX CONCURRENTLY users_email_idx ON users (email)",
            "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS users_email_idx ON users (email, tenant_id); -- comment",
            "DROP INDEX CONCURRENTLY IF EXISTS users_email_idx",
            "DROP INDEX CONCURRENTLY users_email_idx CASCADE",
        ] {
            assert!(
                validate_postgres_nontransactional_statement(sql).is_ok(),
                "expected supported non-transactional DDL to pass: {sql:?}"
            );
        }

        for sql in [
            "CREATE INDEX users_email_idx ON users (email)",
            "CREATE TABLE users (id integer)",
            "DROP TABLE users",
            "VACUUM",
            "CREATE INDEX CONCURRENTLY schema_migration_runs_idx ON schema_migration_runs (run_id)",
            "CREATE INDEX CONCURRENTLY \"UsersEmailIdx\" ON users (email)",
            "CREATE INDEX CONCURRENTLY app.users_email_idx ON users (email)",
            "CREATE INDEX CONCURRENTLY users_email_idx ON app.users (email)",
            "DROP INDEX CONCURRENTLY app.users_email_idx",
            "DROP INDEX CONCURRENTLY users_email_idx, another_idx",
            "CREATE INDEX CONCURRENTLY users_email_idx ON users USING hash (email)",
            "CREATE INDEX CONCURRENTLY users_email_idx ON users (email DESC)",
            "CREATE INDEX CONCURRENTLY users_email_idx ON users (lower(email))",
            "CREATE INDEX CONCURRENTLY users_email_idx ON users (email) WHERE active",
            "CREATE INDEX CONCURRENTLY users_email_idx ON users (email) INCLUDE (name)",
            "CREATE INDEX CONCURRENTLY users_email_idx ON users (\"Email\")",
        ] {
            assert!(
                validate_postgres_nontransactional_statement(sql).is_err(),
                "expected unsupported non-transactional SQL to be rejected: {sql:?}"
            );
        }

        assert_eq!(
            parse_postgres_nontransactional_index_operation(
                "CREATE UNIQUE INDEX CONCURRENTLY users_email_idx ON users (email)"
            )
            .expect("parse supported unique index operation"),
            PostgresNonTransactionalIndexOperation::Create {
                index_name: "users_email_idx".to_owned(),
                table_name: "users".to_owned(),
                unique: true,
                columns: vec!["email".to_owned()],
            }
        );
    }
}
