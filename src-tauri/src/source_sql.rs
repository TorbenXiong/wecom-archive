pub(super) fn normalized_identifier(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub(super) fn choose_column<'a>(columns: &'a [String], candidates: &[&str]) -> Option<&'a str> {
    candidates.iter().find_map(|candidate| {
        let expected = normalized_identifier(candidate);
        columns
            .iter()
            .find(|column| normalized_identifier(column) == expected)
            .map(String::as_str)
    })
}

pub(super) fn table_columns(connection: &rusqlite::Connection, table: &str) -> Vec<String> {
    let sql = format!("PRAGMA table_info({})", quote_identifier(table));
    let Ok(mut statement) = connection.prepare(&sql) else {
        return vec![];
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(1)) else {
        return vec![];
    };
    rows.filter_map(Result::ok).collect()
}

pub(super) fn choose_identifier_columns<'a>(
    columns: &'a [String],
    candidates: &[&str],
) -> Vec<&'a str> {
    candidates
        .iter()
        .filter_map(|candidate| {
            let expected = normalized_identifier(candidate);
            columns
                .iter()
                .find(|column| normalized_identifier(column) == expected)
                .map(String::as_str)
        })
        .collect()
}

pub(super) fn sqlite_value_to_string(value: rusqlite::types::ValueRef<'_>) -> String {
    match value {
        rusqlite::types::ValueRef::Null => String::new(),
        rusqlite::types::ValueRef::Integer(value) => value.to_string(),
        rusqlite::types::ValueRef::Real(value) => value.to_string(),
        rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
            String::from_utf8_lossy(value).into_owned()
        }
    }
}

pub(super) fn sqlite_value_to_bool(value: rusqlite::types::ValueRef<'_>) -> bool {
    match value {
        rusqlite::types::ValueRef::Integer(value) => value == 1,
        rusqlite::types::ValueRef::Real(value) => value == 1.0,
        rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
            let text = String::from_utf8_lossy(value);
            matches!(
                text.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "recalled" | "revoked" | "revoke"
            )
        }
        rusqlite::types::ValueRef::Null => false,
    }
}

pub(super) fn sqlite_value_to_bytes(value: rusqlite::types::ValueRef<'_>) -> Vec<u8> {
    match value {
        rusqlite::types::ValueRef::Null => vec![],
        rusqlite::types::ValueRef::Integer(value) => value.to_string().into_bytes(),
        rusqlite::types::ValueRef::Real(value) => value.to_string().into_bytes(),
        rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
            value.to_vec()
        }
    }
}

pub(super) fn sqlite_value_to_i64(value: rusqlite::types::ValueRef<'_>) -> Option<i64> {
    match value {
        rusqlite::types::ValueRef::Integer(value) => Some(value),
        rusqlite::types::ValueRef::Real(value) => Some(value as i64),
        rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
            std::str::from_utf8(value).ok()?.parse().ok()
        }
        rusqlite::types::ValueRef::Null => None,
    }
}

pub(super) fn normalize_source_timestamp(value: i64) -> i64 {
    match value.unsigned_abs() {
        0..=99_999_999_999 => value.saturating_mul(1_000),
        100_000_000_000..=99_999_999_999_999 => value,
        100_000_000_000_000..=99_999_999_999_999_999 => value / 1_000,
        _ => value / 1_000_000,
    }
}

pub(super) fn source_timestamp_cutoff(cutoff_unix_ms: i64, sample: i64) -> i64 {
    match sample.unsigned_abs() {
        0..=99_999_999_999 => cutoff_unix_ms / 1_000,
        100_000_000_000..=99_999_999_999_999 => cutoff_unix_ms,
        100_000_000_000_000..=99_999_999_999_999_999 => cutoff_unix_ms.saturating_mul(1_000),
        _ => cutoff_unix_ms.saturating_mul(1_000_000),
    }
}
