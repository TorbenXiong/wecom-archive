use crate::sql::{enum_text, fts_query};
use crate::{ArchiveStore, MessageQuery, StoreError};
use archive_domain::MessageV1;
use rusqlite::{OptionalExtension, params};

impl ArchiveStore {
    pub fn query_messages(&self, query: &MessageQuery) -> Result<Vec<MessageV1>, StoreError> {
        // Page over index keys first. Skipped rows never load payload_json,
        // and temporary sorting is limited to the selected page.
        let limit = query.limit.clamp(1, 50_000) as i64;
        let (predicate, mut values) = message_predicate(query)?;

        let order_by = if query.sort_ascending {
            "m.sent_at ASC, m.stable_message_id ASC"
        } else {
            "m.sent_at DESC, m.stable_message_id DESC"
        };
        // Limit and offset are bound values, including for dynamically selected predicates.
        let limit_parameter = values.len() + 1;
        let offset_parameter = values.len() + 2;
        let sql = format!(
            "SELECT m.payload_json FROM messages m JOIN (
                SELECT m.rowid AS message_rowid, m.sent_at, m.stable_message_id
                FROM messages m WHERE {predicate}
                ORDER BY {order_by} LIMIT ?{limit_parameter} OFFSET ?{offset_parameter}
             ) page ON m.rowid = page.message_rowid
             ORDER BY {}",
            order_by.replace("m.", "page.")
        );
        values.push(rusqlite::types::Value::Integer(limit));
        values.push(rusqlite::types::Value::Integer(
            query.offset.min(i64::MAX as u64) as i64,
        ));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values), |row| {
            row.get::<_, String>(0)
        })?;

        rows.map(|row| {
            let json = row?;
            Ok(serde_json::from_str(&json)?)
        })
        .collect()
    }

    /// Read every matching message once within the current read snapshot.
    /// Pagination is deliberately ignored; ordering and all filters are kept.
    /// The callback can stop the scan by returning an error.
    pub fn visit_messages<E: From<StoreError>>(
        &self,
        query: &MessageQuery,
        mut visitor: impl FnMut(MessageV1) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visit_messages_with_projection(query, |message, _, _| visitor(message))
    }

    /// Include stored projection keys so export can detect older revisions
    /// whose payload changed the time or conversation without updating keys.
    pub fn visit_messages_with_projection<E: From<StoreError>>(
        &self,
        query: &MessageQuery,
        mut visitor: impl FnMut(MessageV1, &str, &str) -> Result<(), E>,
    ) -> Result<(), E> {
        let (predicate, values) = message_predicate(query)?;
        let direction = if query.sort_ascending { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT m.payload_json, m.conversation_id, m.sent_at FROM messages m WHERE {predicate}
             ORDER BY m.sent_at {direction}, m.stable_message_id {direction}"
        );
        let mut statement = self.connection.prepare(&sql).map_err(StoreError::from)?;
        let mut rows = statement
            .query(rusqlite::params_from_iter(values))
            .map_err(StoreError::from)?;
        while let Some(row) = rows.next().map_err(StoreError::from)? {
            let payload: String = row.get(0).map_err(StoreError::from)?;
            let message = serde_json::from_str(&payload).map_err(StoreError::from)?;
            let conversation: String = row.get(1).map_err(StoreError::from)?;
            let sent_at: String = row.get(2).map_err(StoreError::from)?;
            visitor(message, &conversation, &sent_at)?;
        }
        Ok(())
    }

    /// Compatibility fallback for legacy payload/projection discrepancies.
    /// Sort only keys in SQLite, then read one payload at a time. The original
    /// filters still apply to stored columns, matching the existing query API.
    pub fn visit_payload_ordered_messages<E: From<StoreError>>(
        &self,
        query: &MessageQuery,
        projected_conversations: &[String],
        actual_conversation: &str,
        mut visitor: impl FnMut(MessageV1) -> Result<(), E>,
    ) -> Result<(), E> {
        use rusqlite::types::Value;
        if projected_conversations.is_empty() {
            return Ok(());
        }
        self.connection
            .create_scalar_function(
                "archive_export_time",
                1,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8
                    | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
                |context| {
                    let text = context.get::<String>(0)?;
                    let time = chrono::DateTime::parse_from_rfc3339(&text)
                        .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?
                        .with_timezone(&chrono::Utc);
                    // Fixed-width epoch keys preserve nanoseconds and extended years.
                    Ok(format!(
                        "{:020}{:010}",
                        time.timestamp() as i128 - i64::MIN as i128,
                        time.timestamp_subsec_nanos()
                    ))
                },
            )
            .map_err(StoreError::from)?;
        let (predicate, mut values) = message_predicate(query)?;
        let mut parameters = Vec::new();
        for id in projected_conversations {
            values.push(Value::Text(id.clone()));
            parameters.push(format!("?{}", values.len()));
        }
        values.push(Value::Text(actual_conversation.into()));
        let direction = if query.sort_ascending { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT m.stable_message_id FROM messages m
             WHERE {predicate} AND m.conversation_id IN ({})
               AND json_extract(m.payload_json, '$.conversation_id') = ?{}
             ORDER BY archive_export_time(json_extract(m.payload_json, '$.sent_at')) {direction},
                      json_extract(m.payload_json, '$.stable_message_id') {direction}",
            parameters.join(","),
            values.len(),
        );
        let mut statement = self.connection.prepare(&sql).map_err(StoreError::from)?;
        let mut rows = statement
            .query(rusqlite::params_from_iter(values))
            .map_err(StoreError::from)?;
        while let Some(row) = rows.next().map_err(StoreError::from)? {
            let id: String = row.get(0).map_err(StoreError::from)?;
            let message = self
                .get_message(&id)?
                .ok_or_else(|| StoreError::Invalid("export message disappeared".into()))?;
            visitor(message)?;
        }
        Ok(())
    }

    pub fn count_messages(&self, query: &MessageQuery) -> Result<u64, StoreError> {
        let (predicate, values) = message_predicate(query)?;
        let sql = format!("SELECT count(*) FROM messages m WHERE {predicate}");
        let count = self
            .connection
            .query_row(&sql, rusqlite::params_from_iter(values), |row| {
                row.get::<_, i64>(0)
            })?;
        Ok(count.max(0) as u64)
    }

    pub fn message_offset_in_conversation(
        &self,
        stable_message_id: &str,
        sort_ascending: bool,
    ) -> Result<Option<u64>, StoreError> {
        let target = self
            .connection
            .query_row(
                "SELECT conversation_id, sent_at FROM messages WHERE stable_message_id = ?1",
                [stable_message_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((conversation_id, sent_at)) = target else {
            return Ok(None);
        };
        let comparison = if sort_ascending {
            "sent_at < ?2 OR (sent_at = ?2 AND stable_message_id < ?3)"
        } else {
            "sent_at > ?2 OR (sent_at = ?2 AND stable_message_id > ?3)"
        };
        let sql = format!(
            "SELECT count(*) FROM messages
             WHERE conversation_id = ?1
               AND ({comparison})"
        );
        let offset = self.connection.query_row(
            &sql,
            params![conversation_id, sent_at, stable_message_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(Some(offset.max(0) as u64))
    }

    pub fn get_message(&self, stable_message_id: &str) -> Result<Option<MessageV1>, StoreError> {
        let payload = self
            .connection
            .query_row(
                "SELECT payload_json FROM messages WHERE stable_message_id = ?1",
                [stable_message_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        payload
            .map(|json| serde_json::from_str(&json).map_err(StoreError::from))
            .transpose()
    }
}

fn message_predicate(
    query: &MessageQuery,
) -> Result<(String, Vec<rusqlite::types::Value>), StoreError> {
    use rusqlite::types::Value;
    let mut conditions = Vec::new();
    let mut values = Vec::new();
    for (column, value) in [
        ("m.conversation_id =", query.conversation_id.clone()),
        ("m.sender_id =", query.participant_id.clone()),
        (
            "m.sent_at >=",
            query.starts_at.map(|value| value.to_rfc3339()),
        ),
        (
            "m.sent_at <=",
            query.ends_at.map(|value| value.to_rfc3339()),
        ),
        (
            "m.message_type =",
            query.message_type.as_ref().map(enum_text).transpose()?,
        ),
    ] {
        if let Some(value) = value {
            values.push(Value::Text(value));
            conditions.push(format!("{column} ?{}", values.len()));
        }
    }
    if query.media_only {
        conditions.push("EXISTS (SELECT 1 FROM message_media mm WHERE mm.stable_message_id = m.stable_message_id)".into());
    }
    if let Some(text) = query
        .text
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        values.push(Value::Text(fts_query(text)));
        let fts_parameter = values.len();
        values.push(Value::Text(like_pattern(text)));
        let body_parameter = values.len();
        values.push(Value::Text(like_pattern(text)));
        let media_parameter = values.len();
        // Materialize candidates once, rather than rerunning MATCH for every message.
        // The LIKE branch completes FTS token matching for partial and CJK text;
        // UNION also prevents a text-and-attachment match from appearing twice.
        conditions.push(format!(
            "m.stable_message_id IN (
                SELECT stable_message_id FROM message_fts WHERE message_fts MATCH ?{fts_parameter}
                UNION
                SELECT stable_message_id FROM messages
                WHERE body_text LIKE ?{body_parameter} ESCAPE '\\'
                UNION
                SELECT mm.stable_message_id FROM media media_search
                JOIN message_media mm ON mm.media_id = media_search.media_id
                WHERE media_search.original_name LIKE ?{media_parameter} ESCAPE '\\'
            )"
        ));
    }
    Ok((
        if conditions.is_empty() {
            "1".into()
        } else {
            conditions.join(" AND ")
        },
        values,
    ))
}

fn like_pattern(text: &str) -> String {
    format!(
        "%{}%",
        text.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}
