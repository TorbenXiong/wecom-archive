use crate::source_keys::{SourceKey, SourceKeyKind, open_source_database};
use crate::source_protobuf::{
    ProtobufValue, decode_wecom_metadata_body, is_opaque_identifier, protobuf_fields, read_varint,
};
use crate::source_sql::{
    choose_column, choose_identifier_columns, normalize_source_timestamp, normalized_identifier,
    quote_identifier, sqlite_value_to_bytes, sqlite_value_to_i64, sqlite_value_to_string,
    table_columns,
};
use crate::sqlite3mc;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Default)]
pub(super) struct DisplayNameIndex {
    pub(super) participants: BTreeMap<String, String>,
    pub(super) participant_priorities: BTreeMap<String, u8>,
    pub(super) conversations: BTreeMap<String, String>,
    pub(super) conversation_priorities: BTreeMap<String, u8>,
    pub(super) conversation_members: BTreeMap<String, BTreeMap<String, String>>,
    pub(super) conversation_types: BTreeMap<String, String>,
    pub(super) participant_kinds: BTreeMap<String, String>,
    pub(super) profiles: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
    pub(super) profile_priorities: BTreeMap<(String, String), u8>,
}

impl DisplayNameIndex {
    pub(super) fn set_participant(&mut self, id: &str, name: String, priority: u8) {
        if self
            .participant_priorities
            .get(id)
            .is_none_or(|current| priority >= *current)
        {
            self.participants.insert(id.to_owned(), name);
            self.participant_priorities.insert(id.to_owned(), priority);
        }
    }

    pub(super) fn set_profile(&mut self, id: &str, key: &str, value: String, priority: u8) {
        let id = id.trim();
        let value = value.trim();
        if id.is_empty() || value.is_empty() {
            return;
        }
        let profile_key = (id.to_owned(), key.to_owned());
        if self
            .profile_priorities
            .get(&profile_key)
            .is_none_or(|current| priority >= *current)
        {
            self.profiles
                .entry(id.to_owned())
                .or_default()
                .insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
            self.profile_priorities.insert(profile_key, priority);
        }
    }
}

pub(super) fn infer_self_sender_id(messages: &[archive_domain::MessageV1]) -> Option<String> {
    infer_self_from_conversations(
        messages
            .iter()
            .map(|message| message.conversation_id.as_str()),
    )
}

pub(super) fn infer_self_from_conversations<'a>(
    conversations: impl Iterator<Item = &'a str>,
) -> Option<String> {
    let mut counts = BTreeMap::<String, usize>::new();
    let mut seen = std::collections::BTreeSet::new();
    for conversation in conversations {
        if !seen.insert(conversation) {
            continue;
        }
        let Some(members) = conversation.strip_prefix("S:") else {
            continue;
        };
        for member in members.split('_').filter(|member| !member.is_empty()) {
            *counts.entry(member.to_owned()).or_default() += 1;
        }
    }
    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    match ranked.as_slice() {
        [(sender_id, count), rest @ ..]
            if rest
                .first()
                .is_none_or(|(_, next_count)| count > next_count) =>
        {
            Some(sender_id.clone())
        }
        _ => None,
    }
}

pub(super) fn direct_conversation_name(
    conversation_id: &str,
    participants: &BTreeMap<String, String>,
    conversation_members: &BTreeMap<String, BTreeMap<String, String>>,
) -> Option<String> {
    let scoped = conversation_members.get(conversation_id);
    let mut names = Vec::new();
    for participant_id in conversation_id.strip_prefix("S:")?.split('_') {
        let Some(name) = participants
            .get(participant_id)
            .or_else(|| scoped.and_then(|members| members.get(participant_id)))
        else {
            continue;
        };
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    (!names.is_empty()).then(|| names.join("、"))
}

pub(super) fn conversation_type(conversation_id: &str, participant_count: usize) -> &'static str {
    if conversation_id.starts_with("S:") {
        "direct"
    } else if conversation_id.starts_with("M:") {
        "application"
    } else if conversation_id.starts_with("O:") {
        "assistant"
    } else if conversation_id.starts_with("T:") {
        "temporary"
    } else if conversation_id.starts_with("Y:") {
        "system"
    } else if conversation_id.starts_with("R:") || participant_count > 2 {
        "group"
    } else {
        "direct"
    }
}

pub(super) fn preferred_scoped_member_name(
    participant_id: &str,
    conversation_members: &BTreeMap<String, BTreeMap<String, String>>,
) -> Option<String> {
    let mut counts = BTreeMap::<String, usize>::new();
    for members in conversation_members.values() {
        let Some(name) = members.get(participant_id) else {
            continue;
        };
        let Some(name) = clean_display_name(name) else {
            continue;
        };
        *counts.entry(name).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
        .map(|(name, _)| name)
}

pub(super) fn open_snapshot_metadata_database(
    path: &Path,
    key: Option<&SourceKey>,
) -> Option<rusqlite::Connection> {
    if let Ok(connection) = open_source_database(path, None) {
        return Some(connection);
    }
    match key {
        Some(SourceKey {
            kind: SourceKeyKind::RawWxSqlite3Key,
            bytes,
            ..
        }) => sqlite3mc::decrypt_raw_wxsqlite3_database(path, bytes)
            .ok()
            .and_then(|plain| open_source_database(&plain, None).ok()),
        Some(key) => open_source_database(path, Some(key)).ok(),
        None => None,
    }
}

pub(super) fn collect_display_names(
    connection: &rusqlite::Connection,
    index: &mut DisplayNameIndex,
) {
    collect_document_conversations(connection, index);
    collect_participant_profiles(connection, index);
    for (table, id_column, name_column, priority) in [
        ("user_table", "id", "name", 100),
        ("wechat_contactV1", "wxid", "name", 80),
        ("USER", "RID", "name", 90),
    ] {
        collect_name_rows_priority(
            connection,
            table,
            id_column,
            name_column,
            &mut index.participants,
            &mut index.participant_priorities,
            priority,
        );
    }
    collect_conversation_member_rows(
        connection,
        "conversation_user_table",
        "conversation_id",
        "user_id",
        "nick_name",
        &mut index.conversation_members,
    );
    collect_name_rows_priority(
        connection,
        "conversation_table",
        "id",
        "name",
        &mut index.conversations,
        &mut index.conversation_priorities,
        100,
    );
    // O:* 会话是面向当前员工的应用私聊。把应用本身也放进参与者目录，
    // 后续才能稳定显示“应用名-用户名”，而不是短 ID。
    let assistant_ids = index
        .conversations
        .iter()
        .filter(|(id, name)| id.starts_with("O:") && !name.trim().is_empty())
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in assistant_ids {
        let name = index.conversations.get(&id).cloned().unwrap_or_default();
        index.set_participant(&id, name, 105);
        index.participant_kinds.insert(id, "bot".into());
    }

    let Ok(mut statement) = connection.prepare(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    ) else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0)) else {
        return;
    };
    let tables = rows.filter_map(Result::ok).collect::<Vec<_>>();
    for table in tables {
        let normalized_table = normalized_identifier(&table);
        let columns = table_columns(connection, &table);
        let name_column = choose_column(
            &columns,
            &[
                "remark_name",
                "remark",
                "display_name",
                "displayname",
                "nickname",
                "nick_name",
                "real_name",
                "name",
                "alias",
            ],
        );
        let Some(name_column) = name_column else {
            continue;
        };
        if normalized_table.contains("conversation")
            && (normalized_table.contains("user") || normalized_table.contains("member"))
            && let (Some(conversation_column), Some(participant_column)) = (
                choose_column(
                    &columns,
                    &[
                        "conversation_id",
                        "conversationid",
                        "session_id",
                        "sessionid",
                    ],
                ),
                choose_column(
                    &columns,
                    &[
                        "participant_id",
                        "user_id",
                        "userid",
                        "member_id",
                        "account_id",
                        "rid",
                        "vid",
                        "wxid",
                        "username",
                        "id",
                    ],
                ),
            )
        {
            collect_conversation_member_rows(
                connection,
                &table,
                conversation_column,
                participant_column,
                name_column,
                &mut index.conversation_members,
            );
            continue;
        }
        if [
            "participant",
            "contact",
            "member",
            "userinfo",
            "user",
            "friend",
            "buddy",
        ]
        .iter()
        .any(|keyword| normalized_table.contains(keyword))
            && !normalized_table.contains("conversation")
        {
            for id_column in choose_identifier_columns(
                &columns,
                &[
                    "participant_id",
                    "user_id",
                    "userid",
                    "member_id",
                    "account_id",
                    "rid",
                    "vid",
                    "wxid",
                    "openid",
                    "open_id",
                    "username",
                    "id",
                ],
            ) {
                collect_name_rows_priority(
                    connection,
                    &table,
                    id_column,
                    name_column,
                    &mut index.participants,
                    &mut index.participant_priorities,
                    20,
                );
            }
        }
        if !["user", "member"]
            .iter()
            .any(|keyword| normalized_table.contains(keyword))
            && ["conversation", "session", "chat", "room", "group"]
                .iter()
                .any(|keyword| normalized_table.contains(keyword))
            && let Some(id_column) = choose_column(
                &columns,
                &[
                    "conversation_id",
                    "session_id",
                    "chat_id",
                    "room_id",
                    "group_id",
                    "id",
                ],
            )
        {
            collect_name_rows_priority(
                connection,
                &table,
                id_column,
                name_column,
                &mut index.conversations,
                &mut index.conversation_priorities,
                20,
            );
        }
    }
}

pub(super) fn collect_participant_profiles(
    connection: &rusqlite::Connection,
    index: &mut DisplayNameIndex,
) {
    let fields: [(&str, &[&str]); 11] = [
        ("mobile", &["mobile", "mobile_phone", "mobilephone"]),
        ("phone", &["phone", "telephone", "tel"]),
        (
            "office_phone",
            &[
                "office_phone",
                "officephone",
                "work_phone",
                "workphone",
                "landline",
            ],
        ),
        ("email", &["email", "mail", "email_address"]),
        (
            "employee_no",
            &["employee_no", "employee_number", "staff_no", "number"],
        ),
        (
            "department",
            &["department", "department_name", "dept_name", "dept"],
        ),
        (
            "organization",
            &[
                "external_corp_name",
                "corp_name",
                "company_name",
                "company",
                "enterprise",
            ],
        ),
        (
            "position",
            &["position", "external_job", "job_title", "title", "job"],
        ),
        ("address", &["address", "work_address", "office_address"]),
        ("english_name", &["english_name", "englishname", "en_name"]),
        ("signature", &["signature", "personal_signature", "sign"]),
    ];
    for (table, id_candidates) in [
        ("user_table", &["id"][..]),
        ("wechat_contactV1", &["wxid"][..]),
        ("USER", &["RID"][..]),
    ] {
        let columns = table_columns(connection, table);
        let Some(id_column) = choose_column(&columns, id_candidates) else {
            continue;
        };
        let selected = fields
            .iter()
            .filter_map(|(key, candidates)| {
                let present = choose_identifier_columns(&columns, candidates);
                (!present.is_empty()).then_some((*key, present))
            })
            .collect::<Vec<_>>();
        if selected.is_empty() {
            continue;
        }
        let mut projection = vec![quote_identifier(id_column)];
        projection.extend(selected.iter().map(|(_, columns)| {
            let alternatives = columns
                .iter()
                .map(|column| {
                    format!(
                        "nullif(trim(CAST({} AS TEXT)), '')",
                        quote_identifier(column)
                    )
                })
                .collect::<Vec<_>>();
            if alternatives.len() == 1 {
                alternatives[0].clone()
            } else {
                format!("coalesce({})", alternatives.join(", "))
            }
        }));
        let sql = format!(
            "SELECT {} FROM {}{} LIMIT 100000",
            projection.join(", "),
            quote_identifier(table),
            choose_column(
                &columns,
                &[
                    "updated_at",
                    "update_time",
                    "modify_time",
                    "last_update_time",
                    "updated"
                ]
            )
            .map(|column| format!(" ORDER BY {} DESC", quote_identifier(column)))
            .unwrap_or_default()
        );
        let Ok(mut statement) = connection.prepare(&sql) else {
            continue;
        };
        let Ok(rows) = statement.query_map([], |row| {
            let id = sqlite_value_to_string(row.get_ref(0)?);
            let mut profile = serde_json::Map::new();
            for (index, (key, _)) in selected.iter().enumerate() {
                let value = sqlite_value_to_string(row.get_ref(index + 1)?);
                if !value.trim().is_empty() && value.len() <= 2048 {
                    profile.insert((*key).into(), serde_json::Value::String(value));
                }
            }
            Ok((id, profile))
        }) else {
            continue;
        };
        for (id, profile) in rows.filter_map(Result::ok) {
            let id = id.trim();
            if id.is_empty() || profile.is_empty() {
                continue;
            }
            let priority = match table {
                "user_table" => 100,
                "USER" => 90,
                _ => 80,
            };
            for (key, value) in profile {
                if let Some(value) = value.as_str() {
                    index.set_profile(id, &key, value.to_owned(), priority);
                }
            }
        }
    }
    collect_protobuf_participant_profiles(connection, index);
    collect_department_profiles(connection, index);
}

pub(super) fn collect_protobuf_participant_profiles(
    connection: &rusqlite::Connection,
    index: &mut DisplayNameIndex,
) {
    let Ok(mut statement) = connection.prepare("SELECT id, pb_content FROM user_table") else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_bytes(row.get_ref(1)?),
        ))
    }) else {
        return;
    };
    for (id, raw) in rows.filter_map(Result::ok) {
        let profile = decode_participant_profile(&id, &raw);
        if profile.is_empty() {
            continue;
        }
        for (key, value) in profile {
            if let Some(value) = value.as_str() {
                index.set_profile(&id, &key, value.to_owned(), 110);
            }
        }
    }
}

pub(super) fn decode_participant_profile(
    id: &str,
    raw: &[u8],
) -> serde_json::Map<String, serde_json::Value> {
    let mut profile = serde_json::Map::new();
    let Some(fields) = protobuf_fields(raw) else {
        return profile;
    };
    // Bind the payload to its directory row before projecting any private fields.
    if !fields.iter().any(|(field, value)| matches!((field, value), (1, ProtobufValue::Integer(value)) if value.to_string() == id)) { return profile; }
    for (field, value) in &fields {
        let ProtobufValue::Bytes(bytes) = value else {
            continue;
        };
        if let Some(text) = profile_text(bytes) {
            let key = match field {
                3 | 17 if text.contains('@') && !text.chars().any(char::is_whitespace) => {
                    Some("email")
                }
                5 => Some("mobile"),
                // Some client versions keep the landline/extension in the
                // compact profile envelope instead of the ordinary columns.
                6 if looks_like_phone(&text) => Some("phone"),
                22 => Some("english_name"),
                50 => Some("signature"),
                // The visible position column is empty on newer clients;
                // field 55 is the corresponding UTF-8 directory attribute.
                55 => Some("position"),
                _ => None,
            };
            if let Some(key) = key {
                profile
                    .entry(key)
                    .or_insert(serde_json::Value::String(text));
            }
        }
        // The directory stores configured attributes as labeled records at 33.1.
        // Parse only this observed envelope; do not treat arbitrary protobuf text
        // (login accounts, URLs, internal IDs) as contact information.
        if *field != 33 {
            continue;
        }
        for (entry_field, entry) in protobuf_fields(bytes).unwrap_or_default() {
            let (1, ProtobufValue::Bytes(entry)) = (entry_field, entry) else {
                continue;
            };
            let Some(attributes) = protobuf_fields(entry) else {
                continue;
            };
            let text_at = |wanted| {
                attributes.iter().find_map(|(field, value)| match value {
                    ProtobufValue::Bytes(raw) if *field == wanted => profile_text(raw),
                    _ => None,
                })
            };
            let (Some(label), Some(value)) = (text_at(1), text_at(2)) else {
                continue;
            };
            if label.len() > 128 || label.chars().any(char::is_control) {
                continue;
            }
            let key = match label.as_str() {
                "手机" | "手机号" | "手机号码" => "mobile".into(),
                "座机" | "办公电话" => "office_phone".into(),
                "电话" | "联系电话" => "phone".into(),
                "邮箱" | "电子邮箱" | "企业邮箱" => "email".into(),
                "工号" | "员工编号" | "员工工号" => "employee_no".into(),
                "部门" => "department".into(),
                "企业" | "公司" => "organization".into(),
                "职位" | "职务" => "position".into(),
                "地址" => "address".into(),
                "英文名" => "english_name".into(),
                _ => format!("custom:{label}"),
            };
            profile.insert(key, serde_json::Value::String(value));
        }
    }
    profile
}

pub(super) fn looks_like_phone(value: &str) -> bool {
    let digits = value
        .chars()
        .filter(|character| character.is_ascii_digit())
        .count();
    (3..=20).contains(&digits)
        && value
            .chars()
            .all(|character| character.is_ascii_digit() || "+-()－— extEXT".contains(character))
}

pub(super) fn profile_text(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    (!text.is_empty()
        && text.len() <= 2048
        && !text
            .chars()
            .any(|value| value.is_control() && value != '\n' && value != '\t'))
    .then(|| text.to_owned())
}

pub(super) fn collect_department_profiles(
    connection: &rusqlite::Connection,
    index: &mut DisplayNameIndex,
) {
    let Ok(mut departments) =
        connection.prepare("SELECT id, name, parent_id FROM department_tableV2")
    else {
        return;
    };
    let Ok(rows) = departments.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_string(row.get_ref(1)?),
            sqlite_value_to_string(row.get_ref(2)?),
        ))
    }) else {
        return;
    };
    let departments = rows
        .filter_map(Result::ok)
        .map(|(id, name, parent)| (id, (name, parent)))
        .collect::<BTreeMap<_, _>>();
    let mut organization_roots = BTreeMap::<String, String>::new();
    for sql in [
        "SELECT u.id, c.root_department_id FROM user_table u JOIN company_group4 c ON c.id = u.corp_id",
        "SELECT u.id, roots.id FROM user_table u JOIN (
           SELECT min(d.id) AS id, d.corpany_id FROM department_tableV2 d
           WHERE NOT EXISTS (SELECT 1 FROM department_tableV2 p WHERE p.id = d.parent_id)
           GROUP BY d.corpany_id HAVING count(*) = 1
         ) roots ON roots.corpany_id = u.corp_id",
    ] {
        if let Ok(mut statement) = connection.prepare(sql)
            && let Ok(rows) = statement.query_map([], |row| {
                Ok((
                    sqlite_value_to_string(row.get_ref(0)?),
                    sqlite_value_to_string(row.get_ref(1)?),
                ))
            })
        {
            for (id, root) in rows.filter_map(Result::ok) {
                if let Some((name, _)) = departments.get(&root)
                    && !name.trim().is_empty()
                {
                    index.set_profile(&id, "organization", name.clone(), 115);
                    organization_roots.entry(id).or_insert(root);
                }
            }
        }
    }
    let Ok(mut statement) = connection.prepare("SELECT user_id, department_id, job FROM user_dept_tableV2 ORDER BY is_main_job DESC, sort, department_id") else { return; };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_string(row.get_ref(1)?),
            sqlite_value_to_string(row.get_ref(2)?),
        ))
    }) else {
        return;
    };
    let mut paths = BTreeMap::<String, Vec<String>>::new();
    for (id, mut department_id, job) in rows.filter_map(Result::ok) {
        if id.trim().is_empty() {
            continue;
        }
        let mut visited = std::collections::BTreeSet::new();
        let mut names = Vec::new();
        while visited.len() < 32 && visited.insert(department_id.clone()) {
            if organization_roots.get(&id) == Some(&department_id) {
                break;
            }
            let Some((name, parent)) = departments.get(&department_id) else {
                break;
            };
            if !name.trim().is_empty() {
                names.push(name.clone());
            }
            department_id = parent.clone();
        }
        names.reverse();
        let path = names.join("/");
        let user_paths = paths.entry(id.clone()).or_default();
        if !path.is_empty() && !user_paths.contains(&path) {
            user_paths.push(path);
        }
        if !job.trim().is_empty() {
            let has_explicit_position = index
                .profiles
                .get(&id)
                .and_then(|profile| profile.get("position"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.trim().is_empty());
            if !has_explicit_position {
                index.set_profile(&id, "position", job, 120);
            }
        }
    }
    for (id, paths) in paths {
        if !paths.is_empty() {
            index.set_profile(&id, "department", paths.join("；"), 120);
        }
    }
}

pub(super) fn collect_document_conversations(
    connection: &rusqlite::Connection,
    index: &mut DisplayNameIndex,
) {
    let Ok(mut statement) =
        connection.prepare("SELECT id, type_content FROM conversation_table WHERE id LIKE 'M:%'")
    else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            sqlite_value_to_bytes(row.get_ref(1)?),
        ))
    }) else {
        return;
    };
    for (id, content) in rows.filter_map(Result::ok) {
        let Some((title, is_report)) = document_conversation_metadata(&id, &content) else {
            continue;
        };
        index.conversations.insert(id.clone(), title);
        index.conversation_priorities.insert(id.clone(), 110);
        index
            .conversation_types
            .insert(id.clone(), "document".into());
        if let Some(entity_id) = id.strip_prefix("M:") {
            index.set_participant(
                entity_id,
                if is_report {
                    "汇报提醒"
                } else {
                    "文档通知"
                }
                .into(),
                110,
            );
            index
                .participant_kinds
                .insert(entity_id.into(), "bot".into());
        }
    }
}

pub(super) fn document_conversation_metadata(id: &str, raw: &[u8]) -> Option<(String, bool)> {
    let entity_id = id.strip_prefix("M:")?.parse::<u64>().ok()?;
    let mut offset = 0;
    let mut entity_matches = false;
    let mut title = None;
    let mut document_url = None;
    while offset < raw.len() {
        let tag = read_varint(raw, &mut offset)?;
        if tag >> 3 == 0 {
            return None;
        }
        match tag & 7 {
            0 => {
                let value = read_varint(raw, &mut offset)?;
                if tag >> 3 == 1 {
                    entity_matches = value == entity_id;
                }
            }
            1 => offset = offset.checked_add(8)?,
            2 => {
                let length = usize::try_from(read_varint(raw, &mut offset)?).ok()?;
                let end = offset.checked_add(length)?;
                let bytes = raw.get(offset..end)?;
                offset = end;
                // Only the observed title/location fields are needed;
                // other fields may contain document access material.
                match tag >> 3 {
                    4 => document_url = std::str::from_utf8(bytes).ok().map(str::to_owned),
                    5 => title = std::str::from_utf8(bytes).ok().and_then(clean_display_name),
                    _ => {}
                }
            }
            5 => offset = offset.checked_add(4)?,
            _ => return None,
        }
        if offset > raw.len() {
            return None;
        }
    }
    let url = document_url?;
    (entity_matches && url.starts_with("https://doc.weixin.qq.com/"))
        .then(|| (title, url.starts_with("https://doc.weixin.qq.com/journal/")))
        .and_then(|(title, report)| title.map(|title| (title, report)))
}

pub(super) fn collect_group_announcements(
    connection: &rusqlite::Connection,
    target: &mut Vec<(String, String, i64)>,
) {
    let columns = table_columns(connection, "conversation_table");
    if !columns.iter().any(|column| column == "id")
        || !columns.iter().any(|column| column == "notice_content")
    {
        return;
    }
    let time_expression = if columns.iter().any(|column| column == "notice_time") {
        "notice_time"
    } else if columns.iter().any(|column| column == "modify_time") {
        "modify_time"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT id, notice_content, {time_expression} FROM conversation_table WHERE notice_content IS NOT NULL"
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_bytes(row.get_ref(1)?),
            sqlite_value_to_i64(row.get_ref(2)?).unwrap_or_default(),
        ))
    }) else {
        return;
    };
    for (conversation_id, raw_body, raw_time) in rows.filter_map(Result::ok) {
        let Some(body) = decode_wecom_metadata_body(&raw_body) else {
            continue;
        };
        if conversation_id.trim().is_empty() || is_opaque_identifier(&body) {
            continue;
        }
        target.push((conversation_id, body, normalize_source_timestamp(raw_time)));
    }
}

pub(super) fn payload_display_name(payload: &serde_json::Value) -> Option<String> {
    ["sender_name", "display_name", "nickname", "nick_name"]
        .into_iter()
        .filter_map(|key| payload.get(key).and_then(serde_json::Value::as_str))
        .find_map(clean_display_name)
}

pub(super) fn clean_display_name(value: &str) -> Option<String> {
    if value
        .chars()
        .any(|character| character.is_control() || character == '\u{fffd}')
    {
        return None;
    }
    let cleaned = value.trim();
    (!cleaned.is_empty() && cleaned != "-").then(|| cleaned.chars().take(128).collect())
}

pub(super) fn collect_name_rows_priority(
    connection: &rusqlite::Connection,
    table: &str,
    id_column: &str,
    name_column: &str,
    target: &mut BTreeMap<String, String>,
    priorities: &mut BTreeMap<String, u8>,
    priority: u8,
) {
    let sql = format!(
        "SELECT {}, {} FROM {} WHERE {} IS NOT NULL AND {} IS NOT NULL LIMIT 100000",
        quote_identifier(id_column),
        quote_identifier(name_column),
        quote_identifier(table),
        quote_identifier(id_column),
        quote_identifier(name_column),
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_string(row.get_ref(1)?),
        ))
    }) else {
        return;
    };
    for (id, raw_name) in rows.filter_map(Result::ok) {
        let id = id.trim();
        let Some(name) = clean_display_name(&raw_name) else {
            continue;
        };
        if !id.is_empty()
            && id != name
            && priorities
                .get(id)
                .is_none_or(|current| priority >= *current)
        {
            target.insert(id.to_owned(), name);
            priorities.insert(id.to_owned(), priority);
        }
    }
}

pub(super) fn collect_conversation_member_rows(
    connection: &rusqlite::Connection,
    table: &str,
    conversation_column: &str,
    participant_column: &str,
    name_column: &str,
    target: &mut BTreeMap<String, BTreeMap<String, String>>,
) {
    let sql = format!(
        "SELECT {}, {}, {} FROM {} WHERE {} IS NOT NULL AND {} IS NOT NULL LIMIT 200000",
        quote_identifier(conversation_column),
        quote_identifier(participant_column),
        quote_identifier(name_column),
        quote_identifier(table),
        quote_identifier(conversation_column),
        quote_identifier(participant_column),
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_string(row.get_ref(1)?),
            sqlite_value_to_string(row.get_ref(2)?),
        ))
    }) else {
        return;
    };
    for (conversation_id, participant_id, raw_name) in rows.filter_map(Result::ok) {
        let conversation_id = conversation_id.trim();
        let participant_id = participant_id.trim();
        if conversation_id.is_empty() || participant_id.is_empty() {
            continue;
        }
        let name = clean_display_name(&raw_name).unwrap_or_default();
        let members = target.entry(conversation_id.to_owned()).or_default();
        if name.is_empty() {
            members.entry(participant_id.to_owned()).or_default();
        } else {
            members
                .entry(participant_id.to_owned())
                .and_modify(|current| {
                    if current.is_empty() {
                        *current = name.clone();
                    }
                })
                .or_insert(name);
        }
    }
}
