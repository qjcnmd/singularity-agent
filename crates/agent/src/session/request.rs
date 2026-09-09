//! 请求观测的不可变内容索引。日志只保存一次相同的消息和工具定义，
//! 观测按条目 ID 引用，公开详情按需还原，不影响模型上下文与执行恢复。

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_model::ModelTurnRequest;

use super::{LedgerRecord, Result, SessionEntry, SessionError};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContext {
    pub request_id: String,
    pub messages: Vec<String>,
    pub tools: String,
    pub model_preferences: Value,
}

#[derive(Default)]
pub(super) struct RequestIndex {
    by_value: HashMap<u64, Vec<usize>>,
    by_id: HashMap<String, usize>,
    contexts: HashMap<String, usize>,
}

fn fingerprint(value: &Value) -> u64 {
    let mut hash = DefaultHasher::new();
    value.hash(&mut hash);
    hash.finish()
}

fn content(entry: &SessionEntry) -> Option<&Value> {
    match entry {
        SessionEntry::Record {
            record: LedgerRecord::RequestContent { value },
            ..
        } => Some(value),
        _ => None,
    }
}

impl RequestIndex {
    pub(super) fn from_entries(entries: &[SessionEntry]) -> Self {
        let mut index = Self::default();
        for (position, entry) in entries.iter().enumerate() {
            index.observe(entry, position);
        }
        index
    }

    pub(super) fn observe(&mut self, entry: &SessionEntry, position: usize) {
        if let SessionEntry::Record {
            record:
                LedgerRecord::ModelRequest {
                    observation,
                    context: Some(_),
                    ..
                },
            ..
        } = entry
        {
            self.contexts.insert(entry.id().to_string(), position);
            if !observation.request_id.is_empty() {
                self.contexts
                    .insert(observation.request_id.clone(), position);
            }
        }
        if let Some(value) = content(entry) {
            self.by_value
                .entry(fingerprint(value))
                .or_default()
                .push(position);
            self.by_id.insert(entry.id().to_string(), position);
        }
    }

    pub(super) fn find(&self, entries: &[SessionEntry], value: &Value) -> Option<String> {
        self.by_value
            .get(&fingerprint(value))?
            .iter()
            .find_map(|&position| {
                let entry = &entries[position];
                (content(entry) == Some(value)).then(|| entry.id().to_string())
            })
    }

    fn value<'a>(&self, entries: &'a [SessionEntry], id: &str) -> Result<&'a Value> {
        self.by_id
            .get(id)
            .and_then(|&position| content(&entries[position]))
            .ok_or_else(|| {
                SessionError::InvalidStructure(format!("request references missing content {id}"))
            })
    }

    pub(super) fn resolve(
        &self,
        entries: &[SessionEntry],
        context: &RequestContext,
    ) -> Result<Value> {
        let messages = context
            .messages
            .iter()
            .map(|id| self.value(entries, id).cloned())
            .collect::<Result<Vec<_>>>()?;
        let value = json!({
            "request_id": context.request_id,
            "messages": messages,
            "tools": self.value(entries, &context.tools)?,
            "model_preferences": context.model_preferences,
        });
        let request: ModelTurnRequest = serde_json::from_value(value)?;
        Ok(serde_json::to_value(request)?)
    }

    pub(super) fn lookup<'a>(
        &self,
        entries: &'a [SessionEntry],
        id: &str,
    ) -> Result<&'a RequestContext> {
        let context = self
            .contexts
            .get(id)
            .and_then(|&position| match &entries[position] {
                SessionEntry::Record {
                    record: LedgerRecord::ModelRequest { context, .. },
                    ..
                } => context.as_deref(),
                _ => None,
            });
        context.ok_or_else(|| {
            SessionError::InvalidStructure(format!("request details not found: {id}"))
        })
    }

    pub(super) fn head(&self, entries: &[SessionEntry], context: &RequestContext) -> Result<Value> {
        let mut messages = Vec::new();
        for id in &context.messages {
            let message = self.value(entries, id)?;
            if matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            ) {
                messages.push(message);
            }
        }
        Ok(
            json!({ "request_id": context.request_id, "messages": messages,
            "tools": self.value(entries, &context.tools)?, "model_preferences": context.model_preferences }),
        )
    }

    pub(super) fn validate(
        &self,
        entries: &[SessionEntry],
        context: &RequestContext,
    ) -> Result<()> {
        for id in context
            .messages
            .iter()
            .chain(std::iter::once(&context.tools))
        {
            self.value(entries, id)?;
        }
        Ok(())
    }
}

pub(super) fn encode_request(
    value: Value,
    mut intern: impl FnMut(Value) -> Result<String>,
) -> Result<RequestContext> {
    let request: ModelTurnRequest = serde_json::from_value(value)?;
    let messages = request
        .messages
        .into_iter()
        .map(|message| intern(serde_json::to_value(message)?))
        .collect::<Result<_>>()?;
    Ok(RequestContext {
        request_id: request.request_id,
        messages,
        tools: intern(serde_json::to_value(request.tools)?)?,
        model_preferences: serde_json::to_value(request.model_preferences)?,
    })
}

/// v5 数据仅在打开边界转换；只读打开不改盘，写打开在持锁期间原子替换为 v6。
/// 原有条目 ID、顺序与可见内容保持不变，新增内容记录位于其首个消费者之前。
pub(super) fn normalize_legacy(entries: Vec<SessionEntry>) -> Result<Vec<SessionEntry>> {
    let mut normalized = Vec::with_capacity(entries.len());
    let mut index = RequestIndex::default();
    for mut entry in entries {
        if let SessionEntry::Record {
            timestamp,
            record:
                LedgerRecord::ModelRequest {
                    observation,
                    context,
                },
            ..
        } = &mut entry
            && let Some(request) = observation.request.take()
        {
            if context.is_some() {
                return Err(SessionError::InvalidStructure(
                    "request has both inline and referenced context".into(),
                ));
            }
            *context = Some(
                encode_request(request, |value| {
                    if let Some(id) = index.find(&normalized, &value) {
                        return Ok(id);
                    }
                    let id = uuid::Uuid::now_v7().to_string();
                    let entry = SessionEntry::Record {
                        id: id.clone(),
                        timestamp: timestamp.clone(),
                        record: LedgerRecord::RequestContent { value },
                    };
                    index.observe(&entry, normalized.len());
                    normalized.push(entry);
                    Ok(id)
                })?
                .into(),
            );
        }
        index.observe(&entry, normalized.len());
        normalized.push(entry);
    }
    Ok(normalized)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session::{SessionManager, test_support::SessionFixture};
    use singularity_model::{ModelMessage, ModelRole};
    use singularity_protocol::{ProviderAttemptStatus, RequestObservation};

    fn request_record(ordinal: u32, messages: Vec<ModelMessage>) -> (LedgerRecord, Value) {
        let request = serde_json::to_value(ModelTurnRequest::new(
            format!("request-{ordinal}"),
            messages,
        ))
        .unwrap();
        (
            LedgerRecord::ModelRequest {
                observation: RequestObservation {
                    request_id: String::new(),
                    request_head: None,
                    purpose: Default::default(),
                    ordinal,
                    attempt: 1,
                    provider: "p".into(),
                    model: "m".into(),
                    status: ProviderAttemptStatus::Ok,
                    duration_ms: 1,
                    input_tokens: None,
                    output_tokens: None,
                    cached_input_tokens: None,
                    error: None,
                    request_error: None,
                    request: Some(request.clone()),
                },
                context: None,
            },
            request,
        )
    }

    fn snapshots(session: &SessionManager) -> Vec<Value> {
        session
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Record {
                    record:
                        LedgerRecord::ModelRequest {
                            observation,
                            context: Some(context),
                        },
                    ..
                } => {
                    assert!(observation.request.is_none());
                    Some(session.request_snapshot(context).unwrap())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn growing_requests_share_content_and_reopen_exactly() {
        let fixture = SessionFixture::new();
        let id = uuid::Uuid::now_v7().to_string();
        let mut session = fixture.create_session(fixture.home(), &id).unwrap();
        let mut messages = vec![ModelMessage::text(ModelRole::System, "system".repeat(1000))];
        let mut expected = Vec::new();
        for ordinal in 1..=30 {
            messages.push(ModelMessage::text(
                ModelRole::User,
                format!("{ordinal} {}", "payload".repeat(1000)),
            ));
            let (record, request) = request_record(ordinal, messages.clone());
            session.append_record(record).unwrap();
            expected.push(request);
        }
        let content_count = session
            .entries()
            .iter()
            .filter(|entry| content(entry).is_some())
            .count();
        assert_eq!(content_count, 32); // system, 30 messages, one tool schema array
        assert_eq!(snapshots(&session), expected);
        let repeated_size: usize = expected
            .iter()
            .map(|value| serde_json::to_vec(value).unwrap().len())
            .sum();
        assert!(std::fs::metadata(session.path()).unwrap().len() < repeated_size as u64 / 5);
        drop(session);
        assert_eq!(snapshots(&fixture.open_read_only(&id).unwrap()), expected);
    }

    #[test]
    fn legacy_read_is_unchanged_and_write_migrates_preserving_ids() {
        let fixture = SessionFixture::new();
        let id = uuid::Uuid::now_v7().to_string();
        let session = fixture.create_session(fixture.home(), &id).unwrap();
        let path = session.path().to_path_buf();
        drop(session);
        let mut header: Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        header["version"] = json!(5);
        let (record, expected) =
            request_record(1, vec![ModelMessage::text(ModelRole::User, "old message")]);
        let entry = SessionEntry::Record {
            id: "original-entry".into(),
            timestamp: "2026-09-08T00:00:00Z".into(),
            record,
        };
        let original = format!("{}\n{}\n", header, serde_json::to_string(&entry).unwrap());
        std::fs::write(&path, &original).unwrap();
        let read = fixture.open_read_only(&id).unwrap();
        assert_eq!(snapshots(&read), vec![expected.clone()]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        drop(read);
        let write = fixture.open_for_repair(&id).unwrap();
        assert_eq!(snapshots(&write), vec![expected]);
        assert!(
            write
                .entries()
                .iter()
                .any(|entry| entry.id() == "original-entry")
        );
        drop(write);
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(migrated.lines().next().unwrap()).unwrap()["version"],
            6
        );
        assert!(fixture.open_read_only(&id).is_ok());
    }

    #[test]
    fn missing_inspection_content_is_rejected_on_write_but_does_not_block_reopen() {
        let fixture = SessionFixture::new();
        let id = uuid::Uuid::now_v7().to_string();
        let mut session = fixture.create_session(fixture.home(), &id).unwrap();
        let (mut record, _) = request_record(1, vec![]);
        if let LedgerRecord::ModelRequest {
            observation,
            context,
        } = &mut record
        {
            observation.request = None;
            *context = Some(
                RequestContext {
                    request_id: "r".into(),
                    messages: vec![],
                    tools: "missing".into(),
                    model_preferences: json!({}),
                }
                .into(),
            );
        }
        let before = std::fs::read(session.path()).unwrap();
        assert!(session.append_record(record.clone()).is_err());
        assert_eq!(std::fs::read(session.path()).unwrap(), before);
        let path = session.path().to_path_buf();
        drop(session);
        let entry = SessionEntry::Record {
            id: "bad".into(),
            timestamp: "2026-09-08T00:00:00Z".into(),
            record,
        };
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "{}",
            serde_json::to_string(&entry).unwrap()
        )
        .unwrap();
        let reopened = fixture.open_read_only(&id).unwrap();
        let SessionEntry::Record {
            record:
                LedgerRecord::ModelRequest {
                    context: Some(context),
                    ..
                },
            ..
        } = reopened.entries().last().unwrap()
        else {
            panic!("request")
        };
        assert!(reopened.request_snapshot(context).is_err());
        drop(reopened);
        let mut writable = fixture.open_for_repair(&id).unwrap();
        writable
            .append_message(crate::message::AgentMessage::text(
                crate::message::AgentMessageRole::User,
                "continue",
            ))
            .unwrap();
    }
}
