use crate::collector_transfer::redact_sensitive_export;
use crate::paths::{
    collection_roots, create_work_root, load_remembered_source_roots, local_data_root,
    remember_source_root, source_roots_config_path,
};
use crate::source_directory::{
    DisplayNameIndex, collect_display_names, collect_group_announcements, conversation_type,
    direct_conversation_name, infer_self_from_conversations, infer_self_sender_id,
    open_snapshot_metadata_database, payload_display_name, preferred_scoped_member_name,
};
use crate::source_keys::{
    SourceKey, SourceKeyKind, choose_collectable_source, open_source_database,
    resolve_collection_key,
};
use crate::source_media::{
    MediaResolver, ensure_media_reference, local_media_paths_from_extras,
    resolve_media_metadata_with_local_paths,
};
use crate::source_messages::readable_message_payload;
use crate::source_protobuf::clean_message_text;
use crate::source_sql::{
    choose_column, normalize_source_timestamp, quote_identifier, source_timestamp_cutoff,
    sqlite_value_to_bool, sqlite_value_to_bytes, sqlite_value_to_i64, sqlite_value_to_string,
    table_columns,
};
use crate::state::{CommandError, internal_error, sanitize_error};
use crate::{diagnostics, sqlite3mc};
use archive_domain::{
    ArchiveBatchV1, ClientExportV1, CollectionScope, ConversationV1, EmployeeNoticeEvidence,
    ExternalContactConsent, MessageType, ParticipantV1, RetentionDirective, SourceAdapter,
    SourceCandidate,
};
use chrono::{TimeZone, Utc};
use message_parser::{RawMessageRow, normalize};
use sha2::{Digest, Sha256};
use source_windows::WindowsSourceAdapter;
use source_windows::roots::normalize_source_root;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) const LOCAL_NOTICE_VERSION: &str = "client-notice.v1";

pub(super) const LOCAL_NOTICE_TEXT: &str = "加密传输本机企业微信聊天记录到服务端";

pub(super) fn snapshot_cache_enabled() -> bool {
    std::env::var("WECOM_ARCHIVE_SNAPSHOT_CACHE").as_deref() == Ok("1")
}

#[derive(Debug)]
pub(super) struct CollectionAuthorization {
    pub(super) notice_version: String,
    pub(super) notice_displayed_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct MediaCollectionOptions {
    pub(super) include_files: bool,
    pub(super) include_images: bool,
}

#[derive(Default)]
pub(super) struct CollectionReadOptions<'a> {
    pub(super) media_options: MediaCollectionOptions,
    pub(super) media_sink: Option<&'a wecom_archive_server::LocalCollectionMediaSink>,
    pub(super) progress: Option<&'a wecom_archive_server::LocalCollectionProgressReporter>,
    pub(super) since_unix_ms: Option<i64>,
    pub(super) snapshot_cache_root: Option<&'a Path>,
}

/// Collects the currently signed-in local WeCom profile for the embedded
/// enterprise server. Working snapshots and key-probe files are removed before
/// returning; periodic calls may retain raw copies in the advisory cache.
pub fn collect_local_export(
    include_media: bool,
    data_redaction: bool,
    source_root: Option<String>,
) -> Result<ClientExportV1, String> {
    collect_local_export_internal(
        include_media,
        data_redaction,
        None,
        source_root,
        std::sync::Arc::new(|_, _, _| {}),
        None,
    )
}

pub fn collect_local_export_with_progress(
    include_media: bool,
    data_redaction: bool,
    since_unix_ms: Option<i64>,
    source_root: Option<String>,
    progress: wecom_archive_server::LocalCollectionProgressReporter,
    media_sink: wecom_archive_server::LocalCollectionMediaSink,
) -> Result<ClientExportV1, String> {
    collect_local_export_internal(
        include_media,
        data_redaction,
        since_unix_ms,
        source_root,
        progress,
        Some(&media_sink),
    )
}

pub(super) fn collect_local_export_internal(
    include_media: bool,
    data_redaction: bool,
    since_unix_ms: Option<i64>,
    source_root: Option<String>,
    progress: wecom_archive_server::LocalCollectionProgressReporter,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
) -> Result<ClientExportV1, String> {
    progress(6, "发现数据源", "正在查找当前登录的企业微信数据。");
    let pasted_resolved_root = source_root
        .as_deref()
        .map(|raw| {
            normalize_source_root(raw)
                .as_deref()
                .and_then(source_windows::roots::resolve_source_root)
                .ok_or_else(|| "数据目录无效或无法访问，请填写本机磁盘上的企业微信存储目录（不能是盘符根目录）。".to_owned())
        })
        .transpose()?;
    let roots = if let Some(root) = pasted_resolved_root.as_ref() {
        vec![root.clone()]
    } else {
        collection_roots(
            None,
            &load_remembered_source_roots(&source_roots_config_path()),
            &source_windows::default_roots(),
        )
    };
    let candidates = source_windows::discover_roots(&roots);
    if candidates.is_empty() {
        return Err(if source_root.is_some() {
            "这个文件夹里没找到企业微信数据库，请填企业微信实际存放聊天记录的那一层目录。"
                .to_owned()
        } else {
            "未发现可支持的本机企业微信数据，请确认客户端已登录。".to_owned()
        });
    }
    progress(15, "读取授权", "已发现数据源，正在准备只读访问。");
    let executable = std::env::current_exe().map_err(|_| internal_error().message)?;
    let data_root = local_data_root(
        executable
            .parent()
            .ok_or_else(|| internal_error().message)?,
        std::env::var_os("WECOM_ARCHIVE_SERVER_DATA").map(PathBuf::from),
    );
    let temporary_root = create_work_root(&data_root, "local").map_err(|error| error.message)?;
    let snapshot_cache_root = data_root.join("snapshot-cache");
    let result = (|| {
        let (candidate, key) = choose_collectable_source(candidates, &mut |source| {
            resolve_collection_key(
                source,
                source.databases.iter().any(|database| database.encrypted),
                &data_root,
            )
        })?;
        progress(25, "创建快照", "正在只读复制数据库及一致性文件。");
        let export = collect_candidate_with_options_since(
            candidate,
            temporary_root.join("work"),
            key.as_ref(),
            CollectionAuthorization {
                notice_version: LOCAL_NOTICE_VERSION.into(),
                notice_displayed_at: Utc::now(),
            },
            CollectionReadOptions {
                media_options: MediaCollectionOptions {
                    include_files: include_media,
                    include_images: include_media,
                },
                media_sink,
                progress: Some(&progress),
                since_unix_ms,
                snapshot_cache_root: (since_unix_ms.is_some() && snapshot_cache_enabled())
                    .then_some(snapshot_cache_root.as_path()),
            },
        )?;
        progress(88, "整理采集结果", "正在校验消息及媒体内容完整性。");
        Ok::<_, CommandError>(if data_redaction {
            redact_sensitive_export(export)?
        } else {
            export
        })
    })();
    let _ = fs::remove_dir_all(&temporary_root);
    let export = result.map_err(|error| error.message)?;
    if let Some(root) = pasted_resolved_root.as_ref()
        && remember_source_root(&source_roots_config_path(), root).is_err()
    {
        diagnostics::write("source_root_persist", "status=failed");
    }
    Ok(export)
}

#[cfg(test)]
pub(super) fn collect_candidate(
    candidate: SourceCandidate,
    work_root: PathBuf,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
) -> Result<ClientExportV1, CommandError> {
    collect_candidate_with_options(
        candidate,
        work_root,
        key,
        authorization,
        MediaCollectionOptions::default(),
        None,
        None,
    )
}

pub(super) fn collect_candidate_with_options(
    candidate: SourceCandidate,
    work_root: PathBuf,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
    media_options: MediaCollectionOptions,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
    progress: Option<&wecom_archive_server::LocalCollectionProgressReporter>,
) -> Result<ClientExportV1, CommandError> {
    collect_candidate_with_options_since(
        candidate,
        work_root,
        key,
        authorization,
        CollectionReadOptions {
            media_options,
            media_sink,
            progress,
            since_unix_ms: None,
            snapshot_cache_root: None,
        },
    )
}

pub(super) fn collect_candidate_with_options_since(
    candidate: SourceCandidate,
    work_root: PathBuf,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
    options: CollectionReadOptions<'_>,
) -> Result<ClientExportV1, CommandError> {
    if candidate
        .databases
        .iter()
        .any(|database| database.encrypted)
        && key.is_none()
    {
        return Err(CommandError {
            code: "SOURCE_KEY_REQUIRED",
            message: "检测到加密数据库，需要手动密钥或已保存的本机密钥。".into(),
            recoverable: true,
        });
    }
    report_collection_progress(
        options.progress,
        30,
        "创建快照",
        "正在复制数据库、WAL 和 SHM 文件。",
    );
    let cached = options
        .snapshot_cache_root
        .map(|root| {
            source_windows::CachedSnapshot::create(
                &candidate,
                &work_root,
                root,
                source_windows::SnapshotOptions::default(),
            )
        })
        .transpose()
        .map_err(|error| CommandError {
            code: "SOURCE_SNAPSHOT_FAILED",
            message: sanitize_error(&error.to_string()),
            recoverable: true,
        })?;
    let uncached;
    let receipt = if let Some(snapshot) = &cached {
        diagnostics::verbose(
            "snapshot_copy",
            &format!("written_bytes={}", snapshot.written_bytes()),
        );
        snapshot.receipt()
    } else {
        uncached = WindowsSourceAdapter
            .snapshot(&candidate, work_root.clone())
            .map_err(|error| CommandError {
                code: "SOURCE_SNAPSHOT_FAILED",
                message: sanitize_error(&error.to_string()),
                recoverable: true,
            })?;
        &uncached
    };
    report_collection_progress(
        options.progress,
        42,
        "打开快照",
        "数据库快照已完成，正在安全打开副本。",
    );
    let result =
        read_snapshot_export_with_options(&candidate, receipt, key, authorization, options);
    if cached.is_none() {
        cleanup_snapshot(&work_root, &receipt.snapshot_root);
    }
    result
}

pub(super) fn read_snapshot_export_with_options(
    candidate: &SourceCandidate,
    receipt: &archive_domain::SnapshotReceipt,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
    options: CollectionReadOptions<'_>,
) -> Result<ClientExportV1, CommandError> {
    let CollectionReadOptions {
        media_options,
        media_sink,
        progress,
        since_unix_ms,
        snapshot_cache_root: _,
    } = options;
    report_collection_progress(
        progress,
        46,
        "解析数据库",
        "正在打开只读快照并识别消息结构。",
    );
    let message_database = receipt
        .copied_files
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("message.db"))
        })
        .ok_or_else(unsupported_source_schema)?;
    let (database_path, database_key) =
        if let Some(SourceKey {
            kind: SourceKeyKind::RawWxSqlite3Key,
            bytes,
            ..
        }) = key
        {
            let plain = sqlite3mc::decrypt_raw_wxsqlite3_database(message_database, bytes)
                .map_err(|_| CommandError {
                    code: "SOURCE_KEY_INVALID",
                    message: "密钥无法解密数据库页；未读取或导出任何消息。".into(),
                    recoverable: true,
                })?;
            (plain, None)
        } else {
            (message_database.to_path_buf(), key)
        };
    let connection =
        open_source_database(&database_path, database_key).map_err(|error| match error {
            sqlite3mc::CipherDatabaseError::InvalidKey => CommandError {
                code: "SOURCE_KEY_INVALID",
                message: "密钥无法打开加密数据库；未读取或导出任何消息。".into(),
                recoverable: true,
            },
            _ => CommandError {
                code: "DECRYPTION_ENGINE_FAILED",
                message: "解密引擎无法安全打开数据库；未读取或导出任何消息。".into(),
                recoverable: true,
            },
        })?;
    let batch_id = uuid::Uuid::new_v4();
    let mut messages = Vec::new();
    let mut media_blobs = Vec::new();
    if media_options.include_files || media_options.include_images {
        report_collection_progress(
            progress,
            52,
            "准备媒体查找",
            "优先使用消息本地路径，需要时再扫描媒体目录。",
        );
    }
    let media_resolver = MediaResolver::new(candidate, media_options);
    report_collection_progress(progress, 60, "读取消息", "正在解析聊天消息及附件引用。");
    let has_normalized = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='messages')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| unsupported_source_schema())?
        != 0;
    if has_normalized {
        let query = if since_unix_ms.is_some() {
            "SELECT source_message_id, conversation_id, sender_id, sent_at_unix_ms,
                    outgoing, raw_type, payload_json, recalled
             FROM messages WHERE sent_at_unix_ms >= ?1
             ORDER BY sent_at_unix_ms, source_message_id"
        } else {
            "SELECT source_message_id, conversation_id, sender_id, sent_at_unix_ms,
                    outgoing, raw_type, payload_json, recalled
             FROM messages ORDER BY sent_at_unix_ms, source_message_id"
        };
        let mut statement = connection
            .prepare(query)
            .map_err(|_| unsupported_source_schema())?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(since_unix_ms), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })
            .map_err(|_| unsupported_source_schema())?;
        for row in rows {
            let (
                source_message_id,
                conversation_id,
                sender_id,
                sent_at_unix_ms,
                outgoing,
                raw_type,
                payload_json,
                recalled,
            ) = row.map_err(|_| unsupported_source_schema())?;
            let payload =
                serde_json::from_str(&payload_json).map_err(|_| unsupported_source_schema())?;
            let mut message = normalize(
                RawMessageRow {
                    source_instance_id: candidate.source_id.clone(),
                    source_message_id,
                    conversation_id,
                    sender_id,
                    sent_at_unix_ms,
                    outgoing: outgoing.map(|value| value != 0),
                    raw_type,
                    payload,
                    recalled: recalled != 0,
                },
                batch_id,
            )
            .map_err(|_| unsupported_source_schema())?;
            ensure_media_reference(&mut message);
            media_blobs.extend(resolve_media_metadata_with_local_paths(
                candidate,
                &media_resolver,
                &mut message,
                media_options,
                media_sink,
                &[],
            )?);
            messages.push(message);
        }
    } else {
        let has_wecom = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='message_table')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|_| unsupported_source_schema())?
            != 0;
        if !has_wecom {
            return Err(unsupported_source_schema());
        }
        let source_cutoff = since_unix_ms.and_then(|cutoff| {
            connection
                .query_row("SELECT MAX(send_time) FROM message_table", [], |row| {
                    Ok(sqlite_value_to_i64(row.get_ref(0)?))
                })
                .ok()
                .flatten()
                .map(|sample| source_timestamp_cutoff(cutoff, sample))
        });
        let message_table_columns = table_columns(&connection, "message_table");
        let local_media_column = if message_table_columns
            .iter()
            .any(|column| column == "local_extra_content")
        {
            "local_extra_content"
        } else {
            "NULL"
        };
        let extra_media_column = if message_table_columns
            .iter()
            .any(|column| column == "extra_content")
        {
            "extra_content"
        } else {
            "NULL"
        };
        let voice_table_columns = table_columns(&connection, "msg_voice2text");
        let voice_text_column = choose_column(&voice_table_columns, &["text"]);
        let voice_message_id_column = choose_column(&voice_table_columns, &["message_id"]);
        let server_id_column = choose_column(&message_table_columns, &["server_id"]);
        let recalled_column = choose_column(
            &message_table_columns,
            &[
                "recalled",
                "is_recalled",
                "revoked",
                "is_revoked",
                "is_revoke",
                "revoke",
            ],
        );
        let recalled_select = recalled_column
            .map(|column| format!("message_table.{}", quote_identifier(column)))
            .unwrap_or_else(|| "NULL".into());
        let voice_join = match (server_id_column, voice_message_id_column, voice_text_column) {
            (Some(server_id), Some(message_id), Some(text)) => format!(
                "LEFT JOIN (SELECT {message_id} AS message_id, MAX({text}) AS text FROM msg_voice2text GROUP BY {message_id}) AS voice ON voice.message_id = message_table.{server_id}",
                message_id = quote_identifier(message_id),
                text = quote_identifier(text),
                server_id = quote_identifier(server_id),
            ),
            _ => String::new(),
        };
        let voice_text_select = if voice_join.is_empty() {
            "NULL".to_owned()
        } else {
            "voice.text".to_owned()
        };
        let predicate = if source_cutoff.is_some() {
            "WHERE send_time >= ?1"
        } else {
            ""
        };
        let query = format!(
            "SELECT message_table.rowid, message_table.send_time,
                    message_table.conversation_id, message_table.sender_id,
                    message_table.content_type, message_table.content,
                    {extra_media_column}, {local_media_column}, {voice_text_select},
                    {recalled_select}
             FROM message_table {voice_join} {predicate}
             ORDER BY message_table.send_time, message_table.rowid",
            extra_media_column = extra_media_column,
            recalled_select = recalled_select,
            local_media_column = local_media_column,
            voice_text_select = voice_text_select,
            voice_join = voice_join,
            predicate = predicate,
        );
        let mut statement = connection
            .prepare(&query)
            .map_err(|_| unsupported_source_schema())?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(source_cutoff), |row| {
                let row_id = sqlite_value_to_string(row.get_ref(0)?);
                let send_time = sqlite_value_to_i64(row.get_ref(1)?).unwrap_or_default();
                let sent_at_unix_ms = normalize_source_timestamp(send_time);
                let conversation_id = sqlite_value_to_string(row.get_ref(2)?);
                let sender_id = match row.get_ref(3)? {
                    rusqlite::types::ValueRef::Null => None,
                    value => Some(sqlite_value_to_string(value)),
                };
                let raw_type = sqlite_value_to_string(row.get_ref(4)?);
                let content = sqlite_value_to_bytes(row.get_ref(5)?);
                let extra_content = sqlite_value_to_bytes(row.get_ref(6)?);
                let local_extra = sqlite_value_to_bytes(row.get_ref(7)?);
                let voice_text = match row.get_ref(8)? {
                    rusqlite::types::ValueRef::Null => None,
                    value => Some(sqlite_value_to_string(value)),
                };
                let recalled = sqlite_value_to_bool(row.get_ref(9)?);
                Ok((
                    row_id,
                    conversation_id,
                    sender_id,
                    sent_at_unix_ms,
                    raw_type,
                    content,
                    extra_content,
                    local_extra,
                    voice_text,
                    recalled,
                ))
            })
            .map_err(|_| unsupported_source_schema())?;
        for row in rows {
            let (
                source_message_id,
                conversation_id,
                sender_id,
                sent_at_unix_ms,
                raw_type,
                content,
                extra_content,
                local_extra,
                voice_text,
                recalled,
            ) = row.map_err(|_| unsupported_source_schema())?;
            if since_unix_ms.is_some_and(|cutoff| sent_at_unix_ms < cutoff) {
                continue;
            }
            let mut payload = readable_message_payload(&raw_type, &content);
            if raw_type.trim() == "16"
                && let Some(transcription) = voice_text.as_deref().and_then(clean_message_text)
                && let serde_json::Value::Object(object) = &mut payload
            {
                object.insert(
                    "transcription".into(),
                    serde_json::Value::String(transcription),
                );
            }
            let local_paths = local_media_paths_from_extras(
                &raw_type,
                &content,
                &[extra_content.as_slice(), local_extra.as_slice()],
            );
            let mut message = normalize(
                RawMessageRow {
                    source_instance_id: candidate.source_id.clone(),
                    source_message_id,
                    conversation_id,
                    sender_id,
                    sent_at_unix_ms,
                    outgoing: None,
                    raw_type,
                    payload,
                    recalled,
                },
                batch_id,
            )
            .map_err(|_| unsupported_source_schema())?;
            ensure_media_reference(&mut message);
            media_blobs.extend(resolve_media_metadata_with_local_paths(
                candidate,
                &media_resolver,
                &mut message,
                media_options,
                media_sink,
                &local_paths,
            )?);
            messages.push(message);
        }
    }

    report_collection_progress(
        progress,
        76,
        "整理消息",
        "正在整理会话、发送人及群成员信息。",
    );

    let inferred_self_sender_id = if since_unix_ms.is_some() {
        // Inferring the employee from only the recent window can turn a direct
        // conversation into an ambiguous pair. Read just distinct directory keys
        // from the snapshot, without parsing historical message bodies or media.
        let table = if has_normalized {
            "messages"
        } else {
            "message_table"
        };
        let mut statement = connection
            .prepare(&format!("SELECT DISTINCT conversation_id FROM {table}"))
            .map_err(|_| unsupported_source_schema())?;
        let ids = statement
            .query_map([], |row| Ok(sqlite_value_to_string(row.get_ref(0)?)))
            .map_err(|_| unsupported_source_schema())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| unsupported_source_schema())?;
        infer_self_from_conversations(ids.iter().map(String::as_str))
    } else {
        infer_self_sender_id(&messages)
    };
    if let Some(self_sender_id) = inferred_self_sender_id.as_deref() {
        for message in &mut messages {
            if message.direction == archive_domain::MessageDirection::Unknown {
                message.direction = if message.sender_id.as_deref() == Some(self_sender_id) {
                    archive_domain::MessageDirection::Outgoing
                } else if message.sender_id.is_some() {
                    archive_domain::MessageDirection::Incoming
                } else {
                    archive_domain::MessageDirection::System
                };
            }
        }
    }

    let mut display_names = DisplayNameIndex::default();
    let mut group_announcements = Vec::new();
    collect_display_names(&connection, &mut display_names);
    let mut metadata_paths = receipt
        .copied_files
        .iter()
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("message.db"))
        })
        .collect::<Vec<_>>();
    metadata_paths.sort_by_key(
        |path| match path.file_name().and_then(|name| name.to_str()) {
            Some(name) if name.eq_ignore_ascii_case("user.db") => 0,
            Some(name) if name.eq_ignore_ascii_case("session.db") => 1,
            _ => 2,
        },
    );
    for metadata_path in metadata_paths {
        if let Some(metadata) = open_snapshot_metadata_database(metadata_path, key) {
            collect_display_names(&metadata, &mut display_names);
            collect_group_announcements(&metadata, &mut group_announcements);
        }
    }
    for (conversation_id, body, sent_at_unix_ms) in group_announcements {
        if messages.iter().any(|message| {
            message.conversation_id == conversation_id
                && message.body_text.as_deref() == Some(body.as_str())
        }) {
            continue;
        }
        let body_hash = hex::encode(Sha256::digest(body.as_bytes()));
        let source_message_id = format!(
            "group-announcement:{}:{}",
            conversation_id,
            &body_hash[..20]
        );
        let mut stable_hasher = Sha256::new();
        stable_hasher.update(b"message.v1\0");
        stable_hasher.update(candidate.source_id.as_bytes());
        stable_hasher.update(b"\0");
        stable_hasher.update(source_message_id.as_bytes());
        messages.push(archive_domain::MessageV1 {
            schema_version: archive_domain::MESSAGE_SCHEMA_VERSION.into(),
            source_kind: "windows_local".into(),
            stable_message_id: hex::encode(stable_hasher.finalize()),
            source_instance_id: candidate.source_id.clone(),
            source_message_id,
            conversation_id,
            sender_id: None,
            sent_at: Utc
                .timestamp_millis_opt(sent_at_unix_ms)
                .single()
                .unwrap_or_else(Utc::now),
            direction: archive_domain::MessageDirection::System,
            message_type: MessageType::System,
            body_text: Some(body.clone()),
            quoted_message_id: None,
            lifecycle: archive_domain::LifecycleState::Active,
            media: Vec::new(),
            raw_type: "group_announcement".into(),
            raw_payload: serde_json::json!({ "kind": "group_announcement", "text": body }),
            parser_version: "windows-group-metadata/0.0.2".into(),
            collection_batch_id: batch_id,
        });
    }
    messages.sort_by(|left, right| {
        left.sent_at
            .cmp(&right.sent_at)
            .then_with(|| left.stable_message_id.cmp(&right.stable_message_id))
    });
    for message in &messages {
        if let (Some(sender_id), Some(name)) = (
            message.sender_id.as_ref(),
            payload_display_name(&message.raw_payload),
        ) {
            display_names.set_participant(sender_id, name, 30);
        }
    }

    let mut conversation_participants = BTreeMap::<String, Vec<String>>::new();
    let mut participant_ids = std::collections::BTreeSet::new();
    let mut outgoing_sender_counts = BTreeMap::<String, usize>::new();
    for message in &messages {
        let members = conversation_participants
            .entry(message.conversation_id.clone())
            .or_default();
        if let Some(sender_id) = &message.sender_id {
            if !members.contains(sender_id) {
                members.push(sender_id.clone());
            }
            participant_ids.insert(sender_id.clone());
            if message.direction == archive_domain::MessageDirection::Outgoing {
                *outgoing_sender_counts.entry(sender_id.clone()).or_default() += 1;
            }
        }
        if display_names
            .participants
            .contains_key(&message.conversation_id)
            && !members.contains(&message.conversation_id)
        {
            members.push(message.conversation_id.clone());
            participant_ids.insert(message.conversation_id.clone());
        }
    }
    for (conversation_id, scoped_members) in &display_names.conversation_members {
        let Some(members) = conversation_participants.get_mut(conversation_id) else {
            continue;
        };
        for member_id in scoped_members.keys() {
            if !members.iter().any(|member| member == member_id) {
                members.push(member_id.clone());
            }
            participant_ids.insert(member_id.clone());
        }
    }
    let self_sender_id = outgoing_sender_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(sender_id, _)| sender_id)
        .or(inferred_self_sender_id);
    for (conversation_id, members) in &mut conversation_participants {
        let Some(direct_members) = conversation_id.strip_prefix("S:") else {
            continue;
        };
        for member_id in direct_members.split('_') {
            if member_id.is_empty() {
                continue;
            }
            if !members.iter().any(|member| member == member_id) {
                members.push(member_id.to_owned());
            }
            participant_ids.insert(member_id.to_owned());
        }
    }
    let conversations = conversation_participants
        .into_iter()
        .map(|(conversation_id, participant_ids)| {
            let conversation_type = display_names
                .conversation_types
                .get(&conversation_id)
                .map(String::as_str)
                .unwrap_or_else(|| conversation_type(&conversation_id, participant_ids.len()));
            let metadata_name = || {
                display_names
                    .conversations
                    .get(&conversation_id)
                    .or_else(|| display_names.participants.get(&conversation_id))
                    .cloned()
            };
            let display_name = if conversation_type == "assistant"
                && display_names.conversations.contains_key(&conversation_id)
            {
                metadata_name().map(|name| {
                    self_sender_id
                        .as_ref()
                        .and_then(|id| display_names.participants.get(id))
                        .map(|employee| format!("{name}-{employee}"))
                        .unwrap_or(name)
                })
            } else if conversation_type == "direct" {
                direct_conversation_name(
                    &conversation_id,
                    &display_names.participants,
                    &display_names.conversation_members,
                )
                .or_else(metadata_name)
            } else if conversation_type == "system" {
                metadata_name().or_else(|| Some("系统通知".into()))
            } else {
                metadata_name()
            };
            ConversationV1 {
                conversation_id,
                display_name,
                conversation_type: Some(conversation_type.into()),
                participant_ids,
            }
        })
        .collect::<Vec<_>>();
    // Carry directory metadata once per conversation, including members who have
    // not sent a message. Keeping it in raw_payload preserves full JSON round trips.
    let members_by_conversation = conversations
        .iter()
        .map(|conversation| {
            (
                conversation.conversation_id.as_str(),
                &conversation.participant_ids,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut profiled_conversations = std::collections::BTreeSet::new();
    for message in &mut messages {
        if !profiled_conversations.insert(message.conversation_id.clone()) {
            continue;
        }
        let Some(members) = members_by_conversation.get(message.conversation_id.as_str()) else {
            continue;
        };
        let profiles = members
            .iter()
            .filter_map(|id| {
                display_names
                    .profiles
                    .get(id)
                    .map(|profile| (id.clone(), serde_json::Value::Object(profile.clone())))
            })
            .collect::<serde_json::Map<_, _>>();
        if !profiles.is_empty()
            && let Some(payload) = message.raw_payload.as_object_mut()
        {
            payload.insert(
                "participant_profiles".into(),
                serde_json::Value::Object(profiles),
            );
        }
    }
    let participants = participant_ids
        .into_iter()
        .map(|participant_id| ParticipantV1 {
            display_name: display_names
                .participants
                .get(&participant_id)
                .cloned()
                .or_else(|| {
                    if Some(participant_id.as_str()) == self_sender_id.as_deref() {
                        None
                    } else {
                        preferred_scoped_member_name(
                            &participant_id,
                            &display_names.conversation_members,
                        )
                    }
                }),
            participant_kind: display_names
                .participant_kinds
                .get(&participant_id)
                .cloned(),
            participant_id,
        })
        .collect::<Vec<_>>();
    let content_sha256 = archive_transfer::json_sha256(&messages).map_err(|_| internal_error())?;
    report_collection_progress(
        progress,
        84,
        "校验媒体",
        "正在校验已采集图片和文件的完整性。",
    );
    media_blobs.sort_by(|left, right| left.content_hash.cmp(&right.content_hash));
    media_blobs.dedup_by(|left, right| left.content_hash == right.content_hash);
    let collected_at = Utc::now();
    let batch = ArchiveBatchV1 {
        schema_version: archive_domain::BATCH_SCHEMA_VERSION.into(),
        batch_id,
        source_adapter: source_windows::ADAPTER_ID.into(),
        source_instance_id: candidate.source_id.clone(),
        collected_at,
        employee_notice: EmployeeNoticeEvidence {
            evidence_hash: notice_evidence_hash(&authorization.notice_version),
            notice_version: authorization.notice_version,
            displayed_at: authorization.notice_displayed_at,
            acknowledged_at: None,
        },
        external_contact_consent: ExternalContactConsent::Unknown,
        collection_scope: CollectionScope {
            source_ids: vec![candidate.source_id.clone()],
            conversation_ids: conversations
                .iter()
                .map(|conversation| conversation.conversation_id.clone())
                .collect(),
            starts_at: None,
            ends_at: None,
            message_types: Vec::<MessageType>::new(),
        },
        cursor_before: None,
        cursor_after: Some(receipt.source_fingerprint.clone()),
        messages,
        content_sha256,
        retention: RetentionDirective {
            policy_id: None,
            expires_at: None,
            legal_hold: false,
        },
    };
    archive_transfer::create_export_with_media(
        candidate.client_version.as_deref().unwrap_or("unverified"),
        vec![batch],
        conversations,
        participants,
        media_blobs,
    )
    .map_err(|error| CommandError {
        code: "COLLECTION_EXPORT_INVALID",
        message: format!(
            "采集结果未通过本地完整性校验：{}",
            sanitize_error(&error.to_string())
        ),
        recoverable: false,
    })
}

pub(super) fn report_collection_progress(
    progress: Option<&wecom_archive_server::LocalCollectionProgressReporter>,
    percent: u8,
    stage: &str,
    detail: &str,
) {
    if let Some(progress) = progress {
        progress(percent, stage, detail);
    }
}

pub(super) fn cleanup_snapshot(work_root: &Path, snapshot_root: &Path) {
    let canonical_work_root = work_root.canonicalize().ok();
    let canonical_parent = snapshot_root
        .parent()
        .and_then(|parent| parent.canonicalize().ok());
    let safe = canonical_work_root.is_some() && canonical_work_root == canonical_parent;
    if safe {
        let _ = fs::remove_dir_all(snapshot_root);
    }
}

pub(super) fn unsupported_source_schema() -> CommandError {
    CommandError {
        code: "SOURCE_SCHEMA_UNSUPPORTED",
        message: "数据结构与当前解析器不匹配；未生成不完整导出。".into(),
        recoverable: true,
    }
}

pub(super) fn notice_evidence_hash(version: &str) -> String {
    hex::encode(Sha256::digest(format!("{version}\n{LOCAL_NOTICE_TEXT}")))
}
