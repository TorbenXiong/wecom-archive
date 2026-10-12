use crate::StoreError;
use rusqlite::Connection;
use serde::Serialize;

pub(super) fn execute_cached(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> rusqlite::Result<usize> {
    connection.prepare_cached(sql)?.execute(parameters)
}

pub(super) fn table_count(connection: &Connection, table: &str) -> Result<u64, StoreError> {
    let sql = match table {
        "conversations" => "SELECT count(*) FROM conversations",
        "messages" => "SELECT count(*) FROM messages",
        "media" => "SELECT count(*) FROM media",
        "audit_events" => "SELECT count(*) FROM audit_events",
        _ => return Err(StoreError::Invalid("invalid count target".into())),
    };
    Ok(connection.query_row(sql, [], |row| row.get::<_, i64>(0))? as u64)
}

pub(super) fn enum_text<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_value(value).and_then(|value| match value {
        serde_json::Value::String(text) => Ok(text),
        _ => serde_json::to_string(&value),
    })
}

pub(super) fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}
