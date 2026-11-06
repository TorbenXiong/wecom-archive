use crate::source_media::{is_media_message_type, media_locator_candidate};
use crate::source_protobuf::{
    ProtobufValue, clean_message_text, decode_wecom_body, decode_wecom_metadata_body,
    decode_wecom_rich_segments, decode_wecom_values, protobuf_fields,
};

pub(super) fn readable_message_payload(raw_type: &str, raw: &[u8]) -> serde_json::Value {
    if raw_type.trim() == "2063" {
        // The event may contain the original payload and unrelated numeric IDs.
        // Until the local target schema is verified, preserve a notice only.
        return serde_json::json!({ "text": "消息已撤回" });
    }
    if matches!(raw_type.trim(), "14" | "29")
        && let Some(payload) = wecom_image_payload(raw_type.trim(), raw)
    {
        return payload;
    }
    if raw_type.trim() == "20"
        && let Some(payload) = wecom_file_payload(raw)
    {
        return payload;
    }
    if let Ok(serde_json::Value::Object(mut payload)) =
        serde_json::from_slice::<serde_json::Value>(raw)
    {
        let values = payload
            .values()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        enrich_media_payload(raw_type, &mut payload, &values);
        return serde_json::Value::Object(payload);
    }
    if raw_type.trim() == "16" {
        return wecom_voice_payload(raw).unwrap_or_else(|| serde_json::json!({}));
    }
    let values = decode_wecom_values(raw);
    let text = if raw_type.trim() == "2" {
        decode_wecom_body(raw)
    } else {
        decode_wecom_metadata_body(raw)
    };
    let mut payload = serde_json::Map::new();
    if let Some(text) = text {
        let key = match raw_type.trim() {
            "15" | "20" => "name",
            "13" => "title",
            _ => "text",
        };
        payload.insert(key.into(), serde_json::Value::String(text));
    }
    if raw_type.trim() == "2"
        && let Some(segments) = decode_wecom_rich_segments(raw)
    {
        payload.insert("rich_segments".into(), serde_json::Value::Array(segments));
    }
    enrich_media_payload(raw_type, &mut payload, &values);
    serde_json::Value::Object(payload)
}

pub(super) fn wecom_voice_payload(raw: &[u8]) -> Option<serde_json::Value> {
    let mut payload = serde_json::json!({"name": "voice.silk", "mime": "audio/silk"});
    // Only field 10 was verified against local file contents. Fields 1 and 8
    // carry remote access metadata and must not be projected into the archive.
    for (field, value) in protobuf_fields(raw)? {
        match (field, value) {
            (10, ProtobufValue::Bytes(bytes)) => {
                if let Ok(digest) = std::str::from_utf8(bytes)
                    && digest.len() == 32
                    && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    let digest = digest.to_ascii_lowercase();
                    payload["name"] = format!("voice-{digest}.silk").into();
                    payload["media_path"] = format!("{digest}.silk").into();
                    payload["media_md5"] = digest.into();
                }
            }
            (4, ProtobufValue::Integer(size)) => payload["size"] = size.into(),
            _ => {}
        }
    }
    Some(payload)
}

pub(super) fn wecom_image_payload(raw_type: &str, raw: &[u8]) -> Option<serde_json::Value> {
    let fields = protobuf_fields(raw)?;
    let mut payload = serde_json::Map::new();
    let mut hints = Vec::new();
    // Project only observed local lookup metadata. Field 1 contains remote
    // access material and must never enter the archive or diagnostics.
    for (field, value) in fields {
        if let ProtobufValue::Bytes(bytes) = value {
            let Some(text) = std::str::from_utf8(bytes).ok() else {
                continue;
            };
            if raw_type == "14" && field == 2 {
                let name = text.replace('\\', "/").rsplit('/').next()?.to_owned();
                if let Some(name) = clean_message_text(&name) {
                    payload.insert("name".into(), name.clone().into());
                    payload.insert("media_path".into(), name.into());
                }
            } else if ((raw_type == "14" && matches!(field, 8 | 10 | 30))
                || (raw_type == "29" && field == 6))
                && text.len() == 32
                && text.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !hints
                    .iter()
                    .any(|hint: &String| hint.eq_ignore_ascii_case(text))
            {
                hints.push(text.to_owned());
            }
        }
    }
    payload.insert("media_lookup_hints".into(), serde_json::json!(hints));
    Some(payload.into())
}

pub(super) fn wecom_file_payload(raw: &[u8]) -> Option<serde_json::Value> {
    let mut name = None;
    let mut size = None;
    let mut md5 = None;
    for (field, value) in protobuf_fields(raw)? {
        match (field, value) {
            (2, ProtobufValue::Bytes(bytes)) => {
                name = std::str::from_utf8(bytes).ok().and_then(clean_message_text)
            }
            (4, ProtobufValue::Integer(value)) => size = Some(value),
            (10, ProtobufValue::Bytes(bytes)) => {
                md5 = std::str::from_utf8(bytes)
                    .ok()
                    .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    .map(str::to_owned)
            }
            _ => {}
        }
    }
    let name = name?;
    let mut payload = serde_json::json!({ "name": name, "media_path": name });
    if let Some(size) = size {
        payload["size"] = size.into();
    }
    if let Some(md5) = md5 {
        payload["media_md5"] = md5.into();
    }
    Some(payload)
}

pub(super) fn enrich_media_payload(
    raw_type: &str,
    payload: &mut serde_json::Map<String, serde_json::Value>,
    values: &[String],
) {
    if is_media_message_type(raw_type)
        && !["media_path", "file_path", "local_path", "path"]
            .iter()
            .any(|key| {
                payload
                    .get(*key)
                    .and_then(serde_json::Value::as_str)
                    .is_some()
            })
        && let Some(locator) = media_locator_candidate(raw_type, values)
    {
        let file_name = locator
            .replace('\\', "/")
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        payload.insert("media_path".into(), serde_json::Value::String(locator));
        if let Some(file_name) = file_name {
            payload.insert("name".into(), serde_json::Value::String(file_name));
        }
    }
    if raw_type.trim() == "49"
        && ["media_path", "file_path", "local_path", "path"]
            .iter()
            .any(|key| {
                payload
                    .get(*key)
                    .and_then(serde_json::Value::as_str)
                    .is_some()
            })
    {
        payload
            .entry("kind")
            .or_insert_with(|| serde_json::Value::String("file".into()));
    }
}
