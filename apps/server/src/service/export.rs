use archive_domain::{MediaIntegrity, MessageV1};
use archive_export::{ExportError, ExportParticipant, ExportSource, ExportStatistics};
use archive_store::{ArchiveReadSnapshot, ArchiveStore, MessageQuery, StoreError};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn redact_sensitive_messages(messages: &mut [archive_domain::MessageV1]) {
    archive_transfer::redact_sensitive_messages(messages);
}

#[derive(Default)]
pub struct ArchiveExportOptions {
    pub query: MessageQuery,
    pub conversation_ids: Vec<String>,
    pub simplify: bool,
    pub pretty: bool,
    pub data_redaction: bool,
    pub conversation_order_ascending: bool,
}

/// Retains conversation/participant metadata, never the full message archive.
pub struct ArchiveExportSource<'a> {
    store: &'a ArchiveStore,
    _snapshot: ArchiveReadSnapshot<'a>,
    options: ArchiveExportOptions,
    conversations: Vec<(String, DateTime<Utc>)>,
    statistics: ExportStatistics,
    participants: Vec<ExportParticipant>,
    conversation_names: BTreeMap<String, String>,
    fallback_groups: BTreeMap<String, BTreeSet<String>>,
}

impl<'a> ArchiveExportSource<'a> {
    pub fn new(
        store: &'a ArchiveStore,
        mut options: ArchiveExportOptions,
    ) -> Result<Self, StoreError> {
        // Selected IDs override a single-conversation query consistently in
        // both the metadata scan and legacy-projection compatibility path.
        if !options.conversation_ids.is_empty() {
            options.query.conversation_id = None;
        }
        let snapshot = store.read_snapshot()?;
        let mut statistics = ExportStatistics::default();
        let mut latest = BTreeMap::<String, DateTime<Utc>>::new();
        let mut all_conversations = BTreeSet::new();
        let mut sender_ids = BTreeSet::new();
        let mut projected_groups = BTreeMap::<String, BTreeSet<String>>::new();
        let mut unordered = BTreeSet::new();
        let mut previous_keys = BTreeMap::<String, (DateTime<Utc>, String)>::new();
        let ids: BTreeSet<_> = options.conversation_ids.iter().cloned().collect();
        let queries = if ids.is_empty() {
            vec![options.query.clone()]
        } else {
            ids.into_iter()
                .map(|id| {
                    let mut query = options.query.clone();
                    query.conversation_id = Some(id);
                    query
                })
                .collect()
        };
        for query in queries {
            store.visit_messages_with_projection::<StoreError>(
                &query,
                |message, projected_conversation, projected_time| {
                    let key = (message.sent_at, message.stable_message_id.clone());
                    if let Some(previous) =
                        previous_keys.insert(message.conversation_id.clone(), key.clone())
                        && if query.sort_ascending {
                            key < previous
                        } else {
                            key > previous
                        }
                    {
                        unordered.insert(message.conversation_id.clone());
                    }
                    projected_groups
                        .entry(message.conversation_id.clone())
                        .or_default()
                        .insert(projected_conversation.into());
                    if projected_conversation != message.conversation_id
                        || projected_time != message.sent_at.to_rfc3339()
                    {
                        unordered.insert(message.conversation_id.clone());
                        // The old group must exclude records moved to a new group.
                        unordered.insert(projected_conversation.to_owned());
                    }
                    all_conversations.insert(message.conversation_id.clone());
                    if let Some(id) = &message.sender_id {
                        sender_ids.insert(id.clone());
                    }
                    statistics.summary_media_count += message.media.len() as u64;
                    if !included(&message, options.simplify) {
                        return Ok(());
                    }
                    latest
                        .entry(message.conversation_id.clone())
                        .and_modify(|value| *value = (*value).max(message.sent_at))
                        .or_insert(message.sent_at);
                    statistics.message_count += 1;
                    statistics.media_count += message.media.len() as u64;
                    statistics.missing_media_count += message
                        .media
                        .iter()
                        .filter(|item| item.integrity != MediaIntegrity::Verified)
                        .count() as u64;
                    Ok(())
                },
            )?;
        }
        let mut directory = BTreeMap::new();
        let mut conversation_names = BTreeMap::new();
        for id in all_conversations {
            if let Some(name) = store.conversation_display_name(&id)? {
                conversation_names.insert(id.clone(), name);
            }
            if options.simplify || options.pretty {
                for participant in store.list_conversation_participants(&id)? {
                    directory.insert(participant.participant_id, participant.display_name);
                }
            }
        }
        let participants = sender_ids
            .into_iter()
            .map(|id| ExportParticipant {
                display_name: directory.get(&id).cloned().flatten(),
                participant_id: id,
                participant_kind: None,
            })
            .collect();
        let mut conversations: Vec<_> = latest.into_iter().collect();
        conversations.sort_by(|left, right| {
            let order = left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0));
            if options.conversation_order_ascending {
                order
            } else {
                order.reverse()
            }
        });
        Ok(Self {
            store,
            _snapshot: snapshot,
            options,
            conversations,
            statistics,
            participants,
            conversation_names,
            fallback_groups: projected_groups
                .into_iter()
                .filter(|(id, _)| unordered.contains(id))
                .collect(),
        })
    }

    pub fn participants(&self) -> &[ExportParticipant] {
        &self.participants
    }
    pub fn conversation_names(&self) -> &BTreeMap<String, String> {
        &self.conversation_names
    }
}

fn included(message: &MessageV1, simplify: bool) -> bool {
    !simplify
        || !matches!(
            message.raw_type.as_str(),
            "group_announcement" | "group_board"
        )
}

enum SourceError {
    Store(StoreError),
    Export(ExportError),
}
impl From<StoreError> for SourceError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl ExportSource for ArchiveExportSource<'_> {
    fn statistics(&self) -> ExportStatistics {
        self.statistics
    }
    fn visit_conversations(
        &self,
        visitor: &mut dyn FnMut(&str) -> Result<(), ExportError>,
    ) -> Result<(), ExportError> {
        for (id, _) in &self.conversations {
            visitor(id)?;
        }
        Ok(())
    }
    fn visit_messages(
        &self,
        id: &str,
        visitor: &mut dyn FnMut(&MessageV1) -> Result<(), ExportError>,
    ) -> Result<(), ExportError> {
        let mut query = self.options.query.clone();
        query.conversation_id = Some(id.into());
        // Redaction depends on the chronologically preceding message, including
        // announcements hidden by simplify. Keep at most two original records.
        let mut pending: Option<MessageV1> = None;
        let mut emit = |message: &MessageV1| {
            if included(message, self.options.simplify) {
                visitor(message).map_err(SourceError::Export)
            } else {
                Ok(())
            }
        };
        let mut handle = |message: MessageV1| {
            if !self.options.data_redaction {
                return emit(&message);
            }
            if query.sort_ascending {
                let original = message.clone();
                let mut pair: Vec<_> = pending
                    .take()
                    .into_iter()
                    .chain(std::iter::once(message))
                    .collect();
                redact_sensitive_messages(&mut pair);
                let current = pair.pop().unwrap();
                pending = Some(original);
                emit(&current)
            } else {
                let previous_pending = pending.replace(message.clone());
                if let Some(newer) = previous_pending {
                    let mut pair = vec![message, newer];
                    redact_sensitive_messages(&mut pair);
                    emit(&pair[1])?;
                }
                Ok(())
            }
        };
        let result = if let Some(projected) = self.fallback_groups.get(id) {
            self.store.visit_payload_ordered_messages::<SourceError>(
                &self.options.query,
                &projected.iter().cloned().collect::<Vec<_>>(),
                id,
                &mut handle,
            )
        } else {
            self.store
                .visit_messages::<SourceError>(&query, &mut handle)
        };
        let result = result.and_then(|()| {
            if self.options.data_redaction
                && !query.sort_ascending
                && let Some(mut oldest) = pending
            {
                redact_sensitive_messages(std::slice::from_mut(&mut oldest));
                emit(&oldest)?;
            }
            Ok(())
        });
        result.map_err(|error| match error {
            SourceError::Export(error) => error,
            SourceError::Store(error) => {
                // Keep private database paths and payloads out of diagnostics.
                let _ = error;
                std::io::Error::other("archive source read failed").into()
            }
        })
    }
}
