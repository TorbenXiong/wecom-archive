pub(super) fn decode_wecom_body(raw: &[u8]) -> Option<String> {
    if let Some(segments) = decode_wecom_rich_segments(raw) {
        let text = segments
            .iter()
            .filter_map(|segment| segment.get("text").and_then(serde_json::Value::as_str))
            .collect::<String>();
        return clean_message_text(&text);
    }
    let values = decode_wecom_values(raw);
    (!values.is_empty()).then(|| values.join("\n"))
}

pub(super) fn decode_wecom_rich_segments(raw: &[u8]) -> Option<Vec<serde_json::Value>> {
    let mut segments = Vec::new();
    for (field, value) in protobuf_fields(raw)? {
        let ProtobufValue::Bytes(fragment) = value else {
            return None;
        };
        if field != 1 {
            return None;
        }
        let mut kind = 0;
        let mut content = None;
        for (field, value) in protobuf_fields(fragment)? {
            match (field, value) {
                (1, ProtobufValue::Integer(value)) => kind = value,
                (2, ProtobufValue::Bytes(value)) => content = Some(value),
                _ => {}
            }
        }
        let fields = protobuf_fields(content?)?;
        let value = fields
            .iter()
            .find_map(|(field, value)| match (field, value) {
                (1, ProtobufValue::Bytes(value)) => Some(*value),
                _ => None,
            })?;
        let text = std::str::from_utf8(value).ok()?;
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return None;
        }
        segments.push(serde_json::json!({
            "kind": if kind == 3 { "emoji" } else { "text" },
            "text": text,
        }));
    }
    (!segments.is_empty()).then_some(segments)
}

pub(super) fn decode_wecom_metadata_body(raw: &[u8]) -> Option<String> {
    let mut values = decode_wecom_values(raw);
    let mut seen = std::collections::BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
    if values.iter().any(|value| !is_opaque_identifier(value)) {
        values.retain(|value| !is_opaque_identifier(value));
    }
    (!values.is_empty()).then(|| values.into_iter().take(12).collect::<Vec<_>>().join("\n"))
}

pub(super) fn decode_wecom_values(raw: &[u8]) -> Vec<String> {
    if let Ok(text) = std::str::from_utf8(raw)
        && text
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
    {
        return clean_message_text(text).into_iter().collect();
    }
    let mut values = Vec::new();
    let _ = parse_protobuf_text(raw, 0, &mut values);
    values
}

pub(super) fn is_opaque_identifier(value: &str) -> bool {
    let compact = value.trim();
    !compact.is_empty()
        && (compact.chars().all(|character| character.is_ascii_digit())
            || (compact.len() >= 24
                && compact
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() || character == '-')))
}

pub(super) enum ProtobufValue<'a> {
    Integer(u64),
    Bytes(&'a [u8]),
    Fixed,
}

pub(super) fn protobuf_fields(raw: &[u8]) -> Option<Vec<(u64, ProtobufValue<'_>)>> {
    let mut offset = 0;
    let mut fields = Vec::new();
    while offset < raw.len() {
        let tag = read_varint(raw, &mut offset)?;
        let field = tag >> 3;
        if field == 0 {
            return None;
        }
        let value = match tag & 7 {
            0 => ProtobufValue::Integer(read_varint(raw, &mut offset)?),
            1 => {
                offset = offset.checked_add(8)?;
                ProtobufValue::Fixed
            }
            2 => {
                let length = usize::try_from(read_varint(raw, &mut offset)?).ok()?;
                let end = offset.checked_add(length)?;
                let segment = raw.get(offset..end)?;
                offset = end;
                ProtobufValue::Bytes(segment)
            }
            5 => {
                offset = offset.checked_add(4)?;
                ProtobufValue::Fixed
            }
            _ => return None,
        };
        if offset > raw.len() {
            return None;
        }
        fields.push((field, value));
    }
    (!fields.is_empty()).then_some(fields)
}

pub(super) fn parse_protobuf_text(
    raw: &[u8],
    depth: usize,
    values: &mut Vec<String>,
) -> Option<()> {
    if raw.is_empty() || depth > 4 {
        return None;
    }
    for (_, value) in protobuf_fields(raw)? {
        if let ProtobufValue::Bytes(segment) = value {
            let mut nested = Vec::new();
            if parse_protobuf_text(segment, depth + 1, &mut nested).is_some() && !nested.is_empty()
            {
                values.extend(nested);
            } else if let Some(text) = readable_protobuf_segment(segment) {
                values.push(text);
            }
        }
    }
    Some(())
}

pub(super) fn read_varint(raw: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *raw.get(*offset)?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

pub(super) fn readable_protobuf_segment(raw: &[u8]) -> Option<String> {
    if raw.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(raw).ok()?;
    let text = clean_message_text(text)?;
    Some(text)
}

pub(super) fn clean_message_text(value: &str) -> Option<String> {
    let cleaned = value
        .chars()
        .filter(|character| {
            *character != '\u{fffd}'
                && (!character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        })
        .collect::<String>();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}
