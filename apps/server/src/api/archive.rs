use crate::{
    ApiError, AppState, ArchiveSummaryResponse, ArchiveUpdateQuery, ArchiveUpdateResponse,
    GlobalSearchQuery, GlobalSearchResponse, MessageCountResponse, MessagePositionResponse,
    MessageResponse, MessagesQuery, PageQuery, PositionQuery, QuotedMessageResponse, authorize,
    parse_message_type, parse_sort_ascending, require_super_admin,
};
use archive_store::ArchiveStore;
use archive_store::MessageQuery;
use axum::Json;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use std::sync::Arc;

pub(crate) async fn archive_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ArchiveSummaryResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let local_collection_available = state.local_collector.is_some();
    let archive_path = Arc::clone(&state.archive_path);
    let summary = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.summary())
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(ArchiveSummaryResponse {
        summary,
        local_collection_available,
    }))
}

pub(crate) async fn archive_updates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ArchiveUpdateQuery>,
) -> Result<Json<ArchiveUpdateResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut receiver = state.archive_update.subscribe();
    if *receiver.borrow() <= query.since {
        receiver.changed().await.map_err(|_| ApiError::store())?;
    }
    Ok(Json(ArchiveUpdateResponse {
        revision: *receiver.borrow(),
    }))
}

pub(crate) async fn collected_users(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<archive_store::CollectedUser>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let archive_path = Arc::clone(&state.archive_path);
    let users = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.list_collected_users())
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(users))
}

pub(crate) async fn list_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Json<Vec<archive_store::ConversationListItem>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let archive_path = Arc::clone(&state.archive_path);
    let conversations = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| {
                store.list_conversations(query.limit.unwrap_or(50), query.offset.unwrap_or(0))
            })
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(conversations))
}

pub(crate) async fn list_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<Vec<MessageResponse>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    if query.conversation_id.trim().is_empty() {
        return Err(ApiError::invalid_query());
    }
    let message_type = query
        .message_type
        .as_deref()
        .map(parse_message_type)
        .transpose()?;
    let sort_ascending = parse_sort_ascending(query.sort.as_deref())?;
    let message_query = MessageQuery {
        conversation_id: Some(query.conversation_id),
        participant_id: query.participant_id,
        text: query.text,
        starts_at: None,
        ends_at: None,
        message_type,
        media_only: query.media_only.unwrap_or(false),
        limit: query.limit.unwrap_or(100),
        offset: query.offset.unwrap_or(0),
        sort_ascending,
    };
    let archive_path = Arc::clone(&state.archive_path);
    let messages = tokio::task::spawn_blocking(move || {
        let store = ArchiveStore::open_read_only(&archive_path).map_err(|_| ApiError::store())?;
        let messages = store
            .query_messages(&message_query)
            .map_err(|_| ApiError::store())?;
        let participants = store
            .list_conversation_participants(
                message_query.conversation_id.as_deref().unwrap_or_default(),
            )
            .map_err(|_| ApiError::store())?
            .into_iter()
            .map(|participant| (participant.participant_id.clone(), participant))
            .collect::<std::collections::BTreeMap<_, _>>();
        messages
            .into_iter()
            .map(|message| {
                let participant = message
                    .sender_id
                    .as_ref()
                    .and_then(|sender_id| participants.get(sender_id));
                let quoted_message = message
                    .quoted_message_id
                    .as_deref()
                    .map(|quoted_id| store.get_message(quoted_id))
                    .transpose()
                    .map_err(|_| ApiError::store())?
                    .flatten()
                    .map(|quoted| {
                        let quoted_name = quoted
                            .sender_id
                            .as_ref()
                            .and_then(|sender_id| participants.get(sender_id))
                            .and_then(|participant| participant.display_name.clone());
                        QuotedMessageResponse {
                            stable_message_id: quoted.stable_message_id,
                            sender_id: quoted.sender_id,
                            sender_name: quoted_name,
                            sent_at: quoted.sent_at,
                            body_text: quoted.body_text,
                            message_type: quoted.message_type,
                        }
                    });
                Ok(MessageResponse {
                    sender_name: participant.and_then(|item| item.display_name.clone()),
                    sender_kind: participant.and_then(|item| item.participant_kind.clone()),
                    message,
                    quoted_message,
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(messages))
}

pub(crate) async fn count_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<MessageCountResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    if query.conversation_id.trim().is_empty() {
        return Err(ApiError::invalid_query());
    }
    let message_type = query
        .message_type
        .as_deref()
        .map(parse_message_type)
        .transpose()?;
    let message_query = MessageQuery {
        conversation_id: Some(query.conversation_id),
        participant_id: query.participant_id,
        text: query.text,
        starts_at: None,
        ends_at: None,
        message_type,
        media_only: query.media_only.unwrap_or(false),
        limit: 1,
        offset: 0,
        sort_ascending: false,
    };
    let archive_path = Arc::clone(&state.archive_path);
    let total = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.count_messages(&message_query))
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(MessageCountResponse { total }))
}

pub(crate) async fn search_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<GlobalSearchQuery>,
) -> Result<Json<Vec<GlobalSearchResponse>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    let search_text = query.q.trim().to_owned();
    let has_filters = query
        .conversation_id
        .as_deref()
        .is_some_and(|id| !id.trim().is_empty())
        || query
            .participant_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty())
        || query.message_type.is_some()
        || query.media_only.unwrap_or(false)
        || query.starts_at.is_some()
        || query.ends_at.is_some();
    if (search_text.is_empty() && !has_filters) || search_text.chars().count() > 200 {
        return Err(ApiError::invalid_query());
    }
    let sort_ascending = parse_sort_ascending(query.sort.as_deref())?;
    let message_type = query
        .message_type
        .as_deref()
        .map(parse_message_type)
        .transpose()?;
    let parse_time =
        |value: Option<&str>| -> Result<Option<chrono::DateTime<chrono::Utc>>, ApiError> {
            value
                .map(|value| {
                    chrono::DateTime::parse_from_rfc3339(value)
                        .map(|time| time.with_timezone(&chrono::Utc))
                        .map_err(|_| ApiError::invalid_query())
                })
                .transpose()
        };
    let starts_at = parse_time(query.starts_at.as_deref())?;
    let ends_at = parse_time(query.ends_at.as_deref())?;
    if starts_at
        .zip(ends_at)
        .is_some_and(|(start, end)| start > end)
    {
        return Err(ApiError::invalid_query());
    }
    let locate = query.locate.unwrap_or(true);
    let archive_path = Arc::clone(&state.archive_path);
    let results = tokio::task::spawn_blocking(move || {
        let store = ArchiveStore::open_read_only(&archive_path).map_err(|_| ApiError::store())?;
        let messages = store
            .query_messages(&MessageQuery {
                conversation_id: query.conversation_id,
                participant_id: query.participant_id,
                text: Some(search_text),
                starts_at,
                ends_at,
                message_type,
                media_only: query.media_only.unwrap_or(false),
                limit: query.limit.unwrap_or(50).clamp(1, 100),
                offset: query.offset.unwrap_or(0),
                sort_ascending,
            })
            .map_err(|_| ApiError::store())?;
        let mut participant_cache = std::collections::BTreeMap::new();
        let mut conversation_cache = std::collections::BTreeMap::<String, Option<String>>::new();
        let mut results = Vec::with_capacity(messages.len());
        for message in messages {
            if !participant_cache.contains_key(&message.conversation_id) {
                let participants = store
                    .list_conversation_participants(&message.conversation_id)
                    .map_err(|_| ApiError::store())?
                    .into_iter()
                    .map(|participant| (participant.participant_id.clone(), participant))
                    .collect::<std::collections::BTreeMap<_, _>>();
                participant_cache.insert(message.conversation_id.clone(), participants);
            }
            let participants = participant_cache
                .get(&message.conversation_id)
                .ok_or_else(ApiError::store)?;
            let participant = message
                .sender_id
                .as_ref()
                .and_then(|sender_id| participants.get(sender_id));
            let quoted_message = message
                .quoted_message_id
                .as_deref()
                .map(|quoted_id| store.get_message(quoted_id))
                .transpose()
                .map_err(|_| ApiError::store())?
                .flatten()
                .map(|quoted| QuotedMessageResponse {
                    sender_name: quoted
                        .sender_id
                        .as_ref()
                        .and_then(|sender_id| participants.get(sender_id))
                        .and_then(|item| item.display_name.clone()),
                    stable_message_id: quoted.stable_message_id,
                    sender_id: quoted.sender_id,
                    sent_at: quoted.sent_at,
                    body_text: quoted.body_text,
                    message_type: quoted.message_type,
                });
            let conversation_name =
                if let Some(name) = conversation_cache.get(&message.conversation_id) {
                    name.clone()
                } else {
                    let name = store
                        .conversation_display_name(&message.conversation_id)
                        .map_err(|_| ApiError::store())?
                        .or_else(|| {
                            message.conversation_id.starts_with("S:").then(|| {
                                participants
                                    .values()
                                    .filter_map(|item| item.display_name.clone())
                                    .collect::<Vec<_>>()
                                    .join("、")
                            })
                        })
                        .filter(|name| !name.is_empty());
                    conversation_cache.insert(message.conversation_id.clone(), name.clone());
                    name
                };
            let offset_in_conversation = if locate {
                Some(
                    store
                        .message_offset_in_conversation(&message.stable_message_id, sort_ascending)
                        .map_err(|_| ApiError::store())?
                        .unwrap_or(0),
                )
            } else {
                None
            };
            results.push(GlobalSearchResponse {
                conversation_name,
                offset_in_conversation,
                message: MessageResponse {
                    sender_name: participant.and_then(|item| item.display_name.clone()),
                    sender_kind: participant.and_then(|item| item.participant_kind.clone()),
                    message,
                    quoted_message,
                },
            });
        }
        Ok::<_, ApiError>(results)
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(results))
}

pub(crate) async fn message_position(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(stable_message_id): AxumPath<String>,
    Query(query): Query<PositionQuery>,
) -> Result<Json<MessagePositionResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    let sort_ascending = parse_sort_ascending(query.sort.as_deref())?;
    let archive_path = Arc::clone(&state.archive_path);
    let response = tokio::task::spawn_blocking(move || {
        let store = ArchiveStore::open_read_only(&archive_path).map_err(|_| ApiError::store())?;
        let message = store
            .get_message(&stable_message_id)
            .map_err(|_| ApiError::store())?
            .ok_or_else(ApiError::invalid_query)?;
        let offset = store
            .message_offset_in_conversation(&message.stable_message_id, sort_ascending)
            .map_err(|_| ApiError::store())?
            .unwrap_or(0);
        Ok::<_, ApiError>(MessagePositionResponse {
            conversation_id: message.conversation_id,
            offset_in_conversation: offset,
        })
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(response))
}

pub(crate) async fn list_conversation_participants(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<Vec<archive_store::ParticipantListItem>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    if conversation_id.trim().is_empty() {
        return Err(ApiError::invalid_query());
    }
    let archive_path = Arc::clone(&state.archive_path);
    let participants = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.list_conversation_participants(&conversation_id))
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(participants))
}
