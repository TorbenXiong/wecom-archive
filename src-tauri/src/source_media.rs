use crate::collection::MediaCollectionOptions;
use crate::source_protobuf::{ProtobufValue, protobuf_fields};
use crate::state::CommandError;
use archive_domain::{MediaIntegrity, MessageType, SourceCandidate};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub(super) struct MediaResolver {
    pub(super) roots: Vec<PathBuf>,
    pub(super) files_by_key: BTreeMap<String, Vec<PathBuf>>,
    pub(super) voice_roots: Vec<PathBuf>,
    pub(super) voice_files_by_md5: OnceLock<BTreeMap<String, Vec<PathBuf>>>,
}

impl MediaResolver {
    pub(super) fn new(candidate: &SourceCandidate, options: MediaCollectionOptions) -> Self {
        let mut root_candidates = candidate.media_roots.clone();
        // The client may place downloaded previews beside the recognized media
        // folders. The source root is already authorized, so include it as a
        // bounded fallback without reading outside the copied account.
        root_candidates.push(candidate.root_path.clone());
        root_candidates.sort();
        root_candidates.dedup();
        let roots = root_candidates
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .collect::<Vec<_>>();
        let mut files_by_key = BTreeMap::new();
        if options.include_files || options.include_images {
            let account_root = candidate.root_path.canonicalize().ok();
            let index_account_root = candidate.media_roots.is_empty()
                || account_root.as_ref().is_some_and(|root| {
                    candidate
                        .media_roots
                        .iter()
                        .filter_map(|path| path.canonicalize().ok())
                        .any(|path| &path == root)
                });
            for root in &roots {
                if !index_account_root && account_root.as_ref().is_some_and(|path| root == path) {
                    continue;
                }
                index_media_directory(root, root, 0, &mut files_by_key);
            }
        }
        let mut voice_roots = std::collections::BTreeSet::new();
        if options.include_files {
            for root in &roots {
                let candidates = [root.clone(), root.join("Cache/Voice"), root.join("Voice")];
                for path in candidates {
                    if !path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.eq_ignore_ascii_case("Voice"))
                    {
                        continue;
                    }
                    if let Ok(path) = path.canonicalize()
                        && path.is_dir()
                        && roots.iter().any(|root| path.starts_with(root))
                    {
                        voice_roots.insert(path);
                    }
                }
            }
        }
        Self {
            roots,
            files_by_key,
            voice_roots: voice_roots.into_iter().collect(),
            voice_files_by_md5: OnceLock::new(),
        }
    }

    pub(super) fn resolve(&self, candidate_root: &Path, locator: &str) -> Option<PathBuf> {
        let locator_path = PathBuf::from(locator);
        let direct_candidates = if locator_path.is_absolute() {
            vec![locator_path]
        } else {
            std::iter::once(candidate_root.join(&locator_path))
                .chain(self.roots.iter().map(|root| root.join(&locator_path)))
                .collect()
        };
        let mut direct_match = None;
        for candidate in direct_candidates {
            if let Some(path) = self.authorized_file(candidate) {
                if direct_match
                    .as_ref()
                    .is_some_and(|existing| existing != &path)
                {
                    return None;
                }
                direct_match = Some(path);
            }
        }
        if direct_match.is_some() {
            return direct_match;
        }
        media_lookup_keys(locator).into_iter().find_map(|key| {
            self.files_by_key
                .get(&key)
                .filter(|paths| paths.len() == 1)
                .and_then(|paths| self.authorized_file(paths[0].clone()))
        })
    }

    pub(super) fn authorized_file(&self, candidate: PathBuf) -> Option<PathBuf> {
        let path = candidate.canonicalize().ok()?;
        (path.is_file() && self.roots.iter().any(|root| path.starts_with(root))).then_some(path)
    }

    pub(super) fn resolve_voice_by_md5(
        &self,
        expected: &str,
        size: Option<u64>,
    ) -> Option<PathBuf> {
        if expected.len() != 32 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let expected = expected.to_ascii_lowercase();
        let index = self.voice_files_by_md5.get_or_init(|| {
            // The cache uses timestamp filenames, so a name index cannot link it
            // to a message. Hash each authorized candidate once per collection.
            let paths = self
                .files_by_key
                .values()
                .flatten()
                .filter(|path| self.voice_roots.iter().any(|root| path.starts_with(root)))
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let mut index = BTreeMap::<String, Vec<PathBuf>>::new();
            for path in paths {
                let Some(path) = self
                    .authorized_file(path)
                    .filter(|path| self.voice_roots.iter().any(|root| path.starts_with(root)))
                else {
                    continue;
                };
                if let Ok(Some((digest, _))) = source_windows::voice::silk_file_md5(&path) {
                    index.entry(digest).or_default().push(path);
                }
            }
            index
        });
        let paths = index.get(&expected).filter(|paths| paths.len() == 1)?;
        let path = self.authorized_file(paths[0].clone())?;
        if !self.voice_roots.iter().any(|root| path.starts_with(root)) {
            return None;
        }
        // A cached lookup must not accept a file replaced since indexing.
        let (digest, actual_size) = source_windows::voice::silk_file_md5(&path).ok()??;
        (digest == expected && size.is_none_or(|size| size == actual_size)).then_some(path)
    }
}

pub(super) fn index_media_directory(
    root: &Path,
    directory: &Path,
    depth: usize,
    index: &mut BTreeMap<String, Vec<PathBuf>>,
) {
    if depth > 32 {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            index_media_directory(root, &path, depth + 1, index);
        } else if file_type.is_file() {
            let mut keys = media_lookup_keys(&entry.file_name().to_string_lossy());
            if let Ok(relative) = path.strip_prefix(root) {
                keys.extend(media_lookup_keys(&relative.to_string_lossy()));
            }
            for key in keys {
                let paths = index.entry(key).or_default();
                if !paths.contains(&path) {
                    paths.push(path.clone());
                }
            }
        }
    }
}

pub(super) fn media_lookup_keys(value: &str) -> Vec<String> {
    let normalized = value
        .trim()
        .trim_matches(|character: char| matches!(character, '"' | '\'' | '<' | '>'))
        .replace('\\', "/");
    let mut keys = Vec::new();
    if !normalized.is_empty() {
        keys.push(normalized.to_ascii_lowercase());
    }
    if let Some(name) = normalized
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
    {
        keys.push(name.to_ascii_lowercase());
        if let Some((stem, _)) = name.rsplit_once('.') {
            keys.push(stem.to_ascii_lowercase());
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    keys.retain(|key| seen.insert(key.clone()));
    keys
}

#[cfg(test)]
pub(super) fn local_media_paths(
    raw_type: &str,
    content: &[u8],
    local_extra: &[u8],
) -> Vec<PathBuf> {
    local_media_paths_from_extras(raw_type, content, &[local_extra])
}

pub(super) fn local_media_paths_from_extras(
    raw_type: &str,
    content: &[u8],
    extras: &[&[u8]],
) -> Vec<PathBuf> {
    if !is_media_message_type(raw_type) {
        return Vec::new();
    }
    let primary_resource = protobuf_fields(content)
        .unwrap_or_default()
        .into_iter()
        .find_map(|(field, value)| {
            if let (1, ProtobufValue::Bytes(bytes)) = (field, value) {
                Some(bytes)
            } else {
                None
            }
        });
    let mut paths = Vec::new();
    for extra in extras {
        for (field, value) in protobuf_fields(extra).unwrap_or_default() {
            let cache = match value {
                ProtobufValue::Bytes(cache) if field == 8 => cache,
                ProtobufValue::Bytes(cache) if field == 1 => cache,
                _ => continue,
            };
            let mut previous = None;
            for (field, value) in protobuf_fields(cache).unwrap_or_default() {
                let (1, ProtobufValue::Bytes(bytes)) = (field, value) else {
                    continue;
                };
                if let Ok(text) = std::str::from_utf8(bytes) {
                    let path = PathBuf::from(text);
                    let local_disk = matches!(path.components().next(),
                        Some(std::path::Component::Prefix(prefix))
                        if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)));
                    if path.is_absolute()
                        && local_disk
                        && !path
                            .components()
                            .any(|part| matches!(part, std::path::Component::ParentDir))
                    {
                        paths.push((
                            previous != primary_resource || primary_resource.is_none(),
                            path,
                        ));
                    }
                }
                previous = Some(bytes);
            }
        }
    }
    paths.sort_by_key(|(secondary, _)| *secondary);
    let mut seen = std::collections::BTreeSet::new();
    paths
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

pub(super) fn is_media_message_type(raw_type: &str) -> bool {
    matches!(
        raw_type.trim().to_ascii_lowercase().as_str(),
        "3" | "4"
            | "14"
            | "29"
            | "image"
            | "img"
            | "15"
            | "16"
            | "20"
            | "49"
            | "49:file"
            | "file"
            | "attachment"
    )
}

pub(super) fn media_locator_candidate(raw_type: &str, values: &[String]) -> Option<String> {
    let allow_plain_name = matches!(
        raw_type.trim().to_ascii_lowercase().as_str(),
        "15" | "16" | "20" | "49" | "49:file" | "file" | "attachment"
    );
    let mut candidates = values
        .iter()
        .flat_map(|value| {
            std::iter::once(value.as_str()).chain(value.split(|character: char| {
                character.is_whitespace()
                    || matches!(character, '"' | '\'' | '<' | '>' | '=' | ';' | ',')
            }))
        })
        .filter_map(|value| {
            let candidate = value.trim().trim_matches(|character: char| {
                matches!(character, '"' | '\'' | '[' | ']' | '(' | ')')
            });
            let score = media_locator_score(candidate, allow_plain_name);
            (score > 0).then(|| (score, candidate.to_owned()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    candidates.into_iter().next().map(|(_, value)| value)
}

pub(super) fn media_locator_score(value: &str, allow_plain_name: bool) -> u8 {
    let value = value.trim();
    if value.is_empty() || value.len() > 1024 {
        return 0;
    }
    let normalized = value.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or_default();
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    let known_extension = extension.as_deref().is_some_and(|extension| {
        matches!(
            extension,
            "jpg"
                | "jpeg"
                | "png"
                | "gif"
                | "webp"
                | "bmp"
                | "heic"
                | "pdf"
                | "doc"
                | "docx"
                | "xls"
                | "xlsx"
                | "ppt"
                | "pptx"
                | "txt"
                | "csv"
                | "zip"
                | "rar"
                | "7z"
                | "mp3"
                | "wav"
                | "mp4"
                | "mov"
                | "dat"
                | "exe"
                | "msi"
                | "msix"
                | "bat"
                | "cmd"
                | "ps1"
        )
    });
    if normalized.contains('/') && known_extension {
        6
    } else if known_extension {
        5
    } else if normalized.contains('/') {
        4
    } else if matches!(value.len(), 32 | 40 | 64)
        && value.chars().all(|character| character.is_ascii_hexdigit())
    {
        3
    } else if allow_plain_name
        && value.len() <= 260
        && !value
            .chars()
            .any(|character| matches!(character, '\n' | '\r' | '\t'))
    {
        1
    } else {
        0
    }
}

pub(super) fn ensure_media_reference(message: &mut archive_domain::MessageV1) {
    if !matches!(
        message.message_type,
        MessageType::Image | MessageType::File | MessageType::Audio | MessageType::Video
    ) || !message.media.is_empty()
    {
        return;
    }
    message.media.push(archive_domain::MediaRefV1 {
        content_hash: None,
        original_name: message
            .raw_payload
            .get("name")
            .or_else(|| message.raw_payload.get("filename"))
            .or_else(|| message.raw_payload.get("file_name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| message.body_text.clone()),
        mime_type: None,
        size_bytes: None,
        source_locator: message.source_message_id.clone(),
        integrity: MediaIntegrity::Missing,
        missing_reason: Some("media_locator_not_present_in_message_payload".into()),
    });
}

pub(super) fn resolve_media_metadata_with_local_paths(
    candidate: &SourceCandidate,
    resolver: &MediaResolver,
    message: &mut archive_domain::MessageV1,
    options: MediaCollectionOptions,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
    local_paths: &[PathBuf],
) -> Result<Vec<archive_domain::MediaBlobV1>, CommandError> {
    let mut blobs = Vec::new();
    if message.media.is_empty()
        || (!options.include_files && !options.include_images)
        || message.message_type == archive_domain::MessageType::Video
    {
        return Ok(blobs);
    }
    for media in &mut message.media {
        let resolved =
            if message.message_type == MessageType::Audio && message.raw_type.trim() == "16" {
                message
                    .raw_payload
                    .get("media_md5")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|digest| resolver.resolve_voice_by_md5(digest, media.size_bytes))
            } else {
                local_paths
                    .iter()
                    .find_map(|path| resolver.authorized_file(path.clone()))
                    .or_else(|| resolver.resolve(&candidate.root_path, &media.source_locator))
                    .or_else(|| {
                        media
                            .original_name
                            .as_deref()
                            .and_then(|name| resolver.resolve(&candidate.root_path, name))
                    })
                    .or_else(|| {
                        message
                            .raw_payload
                            .get("media_md5")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|md5| resolver.resolve(&candidate.root_path, md5))
                    })
                    .or_else(|| {
                        message
                            .raw_payload
                            .get("media_lookup_hints")
                            .and_then(serde_json::Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(serde_json::Value::as_str)
                            .find_map(|hint| resolver.resolve(&candidate.root_path, hint))
                    })
                    .or_else(|| resolver.resolve(&candidate.root_path, &message.source_message_id))
            };
        match resolved
            .as_deref()
            .and_then(|path| media_store::hash_source(path).ok())
        {
            Some((hash, size)) => {
                let should_embed = match message.message_type {
                    archive_domain::MessageType::Image => options.include_images,
                    archive_domain::MessageType::File | archive_domain::MessageType::Audio => {
                        options.include_files
                    }
                    _ => false,
                };
                if should_embed && let Some(source_path) = resolved.as_deref() {
                    if let Some(sink) = media_sink {
                        sink(source_path, &hash, size).map_err(|_| CommandError {
                            code: "MEDIA_STORE_FAILED",
                            message: "媒体文件写入归档失败。".into(),
                            recoverable: true,
                        })?;
                    } else if let Ok(bytes) = fs::read(source_path)
                        && bytes.len() as u64 == size
                    {
                        blobs.push(archive_domain::MediaBlobV1 {
                            content_hash: hash.clone(),
                            original_name: media.original_name.clone(),
                            mime_type: media.mime_type.clone(),
                            size_bytes: size,
                            content_hex: hex::encode(bytes),
                        });
                    }
                }
                media.content_hash = Some(hash.clone());
                media.size_bytes = Some(size);
                media.source_locator = format!("sha256:{hash}");
                media.integrity = MediaIntegrity::Verified;
                media.missing_reason = None;
            }
            None => {
                media.content_hash = None;
                media.source_locator = "unavailable".into();
                media.integrity = MediaIntegrity::Missing;
                media.missing_reason = Some("not_found_in_authorized_media_roots".into());
            }
        }
    }
    blobs.sort_by(|left, right| left.content_hash.cmp(&right.content_hash));
    blobs.dedup_by(|left, right| left.content_hash == right.content_hash);
    Ok(blobs)
}
