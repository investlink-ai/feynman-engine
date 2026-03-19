use crate::{BusError, BusMessage, ConsumerGroupInfo, MessageBus, MessageId, Result, TopicInfo};
use chrono::{DateTime, TimeZone, Utc};
use fred::prelude::*;
use fred::types::XReadResponse;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{error, warn};

const SUBSCRIPTION_CHANNEL_CAPACITY: usize = 1_024;
const STREAM_READ_COUNT: u64 = 128;
const STREAM_BLOCK_MS: u64 = 1_000;
const STREAM_GROUP_START_ID: &str = "0-0";
const PENDING_PAGE_SIZE: u64 = 256;
const SUBSCRIBE_RETRY_DELAY_MS: u64 = 250;
const PAYLOAD_FIELD: &str = "payload_hex";
const PUBLISHED_AT_FIELD: &str = "published_at_ms";

type StreamFields = HashMap<String, String>;
type StreamReadResponse = XReadResponse<String, String, String, String>;
type PendingEntry = (String, String, u64, u64);

/// Redis Streams-backed implementation of `MessageBus`.
#[derive(Clone)]
pub struct RedisBus {
    client: RedisClient,
}

impl RedisBus {
    pub async fn connect(url: &str) -> Result<Self> {
        let config = RedisConfig::from_url(url)
            .map_err(|error| BusError::ConnectionFailed(error.to_string()))?;
        let client = Builder::from_config(config)
            .build()
            .map_err(|error| BusError::ConnectionFailed(error.to_string()))?;

        client
            .init()
            .await
            .map_err(|error| BusError::ConnectionFailed(error.to_string()))?;
        client
            .exists::<u64, _>("__feynman_bus_health__")
            .await
            .map_err(|error| BusError::ConnectionFailed(error.to_string()))?;

        Ok(Self { client })
    }

    async fn ensure_group(&self, topic: &str, group: &str) -> Result<()> {
        match self
            .client
            .xgroup_create::<(), _, _, _>(topic, group, STREAM_GROUP_START_ID, true)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if is_busy_group_error(&error) => Ok(()),
            Err(error) if is_no_such_key_error(&error) => {
                Err(BusError::TopicNotFound(topic.to_owned()))
            }
            Err(error) => Err(BusError::SubscribeFailed {
                topic: topic.to_owned(),
                group: group.to_owned(),
                consumer: String::new(),
                reason: error.to_string(),
            }),
        }
    }

    async fn fetch_message(&self, topic: &str, id: &str) -> Result<Option<BusMessage>> {
        let entries = self
            .client
            .xrange_values::<String, String, String, _, _, _>(topic, id, id, Some(1))
            .await
            .map_err(|error| BusError::PendingFailed {
                topic: topic.to_owned(),
                group: String::from("<lookup>"),
                reason: error.to_string(),
            })?;

        entries
            .into_iter()
            .next()
            .map(|(entry_id, fields)| stream_entry_to_message(topic, entry_id, fields))
            .transpose()
    }

    async fn fetch_messages(&self, topic: &str, ids: &[MessageId]) -> Result<Vec<BusMessage>> {
        let mut messages = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(message) = self.fetch_message(topic, &id.0).await? {
                messages.push(message);
            }
        }
        Ok(messages)
    }
}

#[async_trait::async_trait]
impl MessageBus for RedisBus {
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<MessageId> {
        let published_at_ms = Utc::now().timestamp_millis().to_string();
        let fields = vec![
            (PAYLOAD_FIELD.to_owned(), encode_hex(payload)),
            (PUBLISHED_AT_FIELD.to_owned(), published_at_ms),
        ];

        let id = self
            .client
            .xadd::<String, _, _, _, _>(topic, false, None::<()>, "*", fields)
            .await
            .map_err(|error| BusError::PublishFailed {
                topic: topic.to_owned(),
                reason: error.to_string(),
            })?;

        Ok(MessageId(id))
    }

    async fn subscribe(
        &self,
        topic: &str,
        group: &str,
        consumer: &str,
    ) -> Result<mpsc::Receiver<BusMessage>> {
        self.ensure_group(topic, group)
            .await
            .map_err(|error| match error {
                BusError::SubscribeFailed { reason, .. } => BusError::SubscribeFailed {
                    topic: topic.to_owned(),
                    group: group.to_owned(),
                    consumer: consumer.to_owned(),
                    reason,
                },
                other => other,
            })?;

        let (tx, rx) = mpsc::channel(SUBSCRIPTION_CHANNEL_CAPACITY);
        let client = self.client.clone();
        let topic_name = topic.to_owned();
        let group_name = group.to_owned();
        let consumer_name = consumer.to_owned();

        tokio::spawn(async move {
            loop {
                let read_result: RedisResult<StreamReadResponse> = client
                    .xreadgroup_map(
                        &group_name,
                        &consumer_name,
                        Some(STREAM_READ_COUNT),
                        Some(STREAM_BLOCK_MS),
                        false,
                        vec![topic_name.clone()],
                        vec![String::from(">")],
                    )
                    .await;

                match read_result {
                    Ok(records) => {
                        let Some(entries) = records.get(&topic_name) else {
                            continue;
                        };

                        for (id, fields) in entries {
                            let message = match stream_entry_to_message(
                                &topic_name,
                                id.clone(),
                                fields.clone(),
                            ) {
                                Ok(message) => message,
                                Err(error) => {
                                    error!(
                                        topic = %topic_name,
                                        group = %group_name,
                                        consumer = %consumer_name,
                                        error = %error,
                                        "failed to decode redis stream message"
                                    );
                                    continue;
                                }
                            };

                            if tx.send(message).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) if is_no_group_error(&error) => {
                        error!(
                            topic = %topic_name,
                            group = %group_name,
                            consumer = %consumer_name,
                            error = %error,
                            "consumer group disappeared while subscribed"
                        );
                        return;
                    }
                    Err(error) => {
                        warn!(
                            topic = %topic_name,
                            group = %group_name,
                            consumer = %consumer_name,
                            error = %error,
                            "redis stream read failed, retrying"
                        );
                        sleep(Duration::from_millis(SUBSCRIBE_RETRY_DELAY_MS)).await;
                    }
                }
            }
        });

        Ok(rx)
    }

    async fn ack(&self, topic: &str, group: &str, msg_id: &MessageId) -> Result<()> {
        let acked = self
            .client
            .xack::<u64, _, _, _>(topic, group, vec![msg_id.0.clone()])
            .await
            .map_err(|error| map_group_or_ack_error(topic, group, msg_id, error))?;

        if acked == 0 {
            return Err(BusError::AckFailed {
                topic: topic.to_owned(),
                group: group.to_owned(),
                message_id: msg_id.clone(),
                reason: "message was not pending in the consumer group".to_owned(),
            });
        }

        Ok(())
    }

    async fn pending(
        &self,
        topic: &str,
        group: &str,
        min_idle: Duration,
    ) -> Result<Vec<BusMessage>> {
        let min_idle_ms = duration_millis(min_idle, topic, group)?;
        let mut start = String::from("-");
        let mut ids = Vec::new();

        loop {
            let page = self
                .client
                .xpending::<Vec<PendingEntry>, _, _, _>(
                    topic,
                    group,
                    (
                        min_idle_ms,
                        start.clone(),
                        String::from("+"),
                        PENDING_PAGE_SIZE,
                    ),
                )
                .await
                .map_err(|error| map_group_or_pending_error(topic, group, error))?;

            if page.is_empty() {
                break;
            }

            let page_len = page.len();
            for (id, _, _, _) in page {
                start = format!("({id}");
                ids.push(MessageId(id));
            }

            if page_len < PENDING_PAGE_SIZE as usize {
                break;
            }
        }

        self.fetch_messages(topic, &ids).await
    }

    async fn claim(
        &self,
        topic: &str,
        group: &str,
        consumer: &str,
        min_idle: Duration,
        msg_ids: &[MessageId],
    ) -> Result<Vec<BusMessage>> {
        if msg_ids.is_empty() {
            return Ok(Vec::new());
        }

        let min_idle_ms = duration_millis(min_idle, topic, group)?;
        let claimed = self
            .client
            .xclaim_values::<String, String, String, _, _, _, _>(
                topic,
                group,
                consumer,
                min_idle_ms,
                ids_to_strings(msg_ids),
                None,
                None,
                None,
                false,
                false,
            )
            .await
            .map_err(|error| BusError::ClaimFailed {
                topic: topic.to_owned(),
                group: group.to_owned(),
                consumer: consumer.to_owned(),
                reason: error.to_string(),
            })?;

        claimed
            .into_iter()
            .map(|(id, fields)| stream_entry_to_message(topic, id, fields))
            .collect()
    }

    async fn topic_info(&self, topic: &str) -> Result<TopicInfo> {
        let length = self
            .client
            .xlen::<u64, _>(topic)
            .await
            .map_err(|error| map_topic_info_error(topic, error))?;
        let consumer_groups = self
            .client
            .xinfo_groups::<Vec<HashMap<String, RedisValue>>, _>(topic)
            .await
            .map_err(|error| map_topic_info_error(topic, error))?
            .into_iter()
            .map(|values| consumer_group_from_value_map(topic, values))
            .collect::<Result<Vec<_>>>()?;
        let oldest_message = self
            .client
            .xrange_values::<String, String, String, _, _, _>(topic, "-", "+", Some(1))
            .await
            .map_err(|error| map_topic_info_error(topic, error))?
            .into_iter()
            .next()
            .map(|(_, fields)| published_at_from_fields(topic, None, &fields))
            .transpose()?;
        let newest_message = self
            .client
            .xrevrange_values::<String, String, String, _, _, _>(topic, "+", "-", Some(1))
            .await
            .map_err(|error| map_topic_info_error(topic, error))?
            .into_iter()
            .next()
            .map(|(_, fields)| published_at_from_fields(topic, None, &fields))
            .transpose()?;

        Ok(TopicInfo {
            length,
            consumer_groups,
            oldest_message,
            newest_message,
        })
    }

    async fn health_check(&self) -> Result<()> {
        self.client
            .exists::<u64, _>("__feynman_bus_health__")
            .await
            .map(|_| ())
            .map_err(|error| BusError::HealthCheckFailed(error.to_string()))
    }
}

fn duration_millis(min_idle: Duration, topic: &str, group: &str) -> Result<u64> {
    u64::try_from(min_idle.as_millis()).map_err(|_| BusError::PendingFailed {
        topic: topic.to_owned(),
        group: group.to_owned(),
        reason: "idle duration exceeds Redis millisecond range".to_owned(),
    })
}

fn ids_to_strings(ids: &[MessageId]) -> Vec<String> {
    ids.iter().map(|id| id.0.clone()).collect()
}

fn stream_entry_to_message(topic: &str, id: String, fields: StreamFields) -> Result<BusMessage> {
    let message_id = MessageId(id);
    let payload_hex = required_field(topic, &message_id, &fields, PAYLOAD_FIELD)?;
    let payload = decode_hex(topic, &message_id, payload_hex)?;
    let published_at = published_at_from_fields(topic, Some(&message_id), &fields)?;

    Ok(BusMessage {
        id: message_id,
        topic: topic.to_owned(),
        payload,
        published_at,
    })
}

fn published_at_from_fields(
    topic: &str,
    message_id: Option<&MessageId>,
    fields: &StreamFields,
) -> Result<DateTime<Utc>> {
    let field_id = message_id
        .cloned()
        .unwrap_or_else(|| MessageId(String::from("unknown")));
    let published_at_ms = required_field(topic, &field_id, fields, PUBLISHED_AT_FIELD)?;
    let millis = published_at_ms
        .parse::<i64>()
        .map_err(|error| BusError::InvalidMessage {
            topic: topic.to_owned(),
            message_id: field_id.clone(),
            reason: format!("invalid published_at_ms field: {error}"),
        })?;

    Utc.timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| BusError::InvalidMessage {
            topic: topic.to_owned(),
            message_id: field_id,
            reason: format!("published_at_ms {millis} is outside the valid UTC range"),
        })
}

fn required_field<'a>(
    topic: &str,
    message_id: &MessageId,
    fields: &'a StreamFields,
    field: &'static str,
) -> Result<&'a str> {
    fields
        .get(field)
        .map(String::as_str)
        .ok_or_else(|| BusError::MissingField {
            topic: topic.to_owned(),
            message_id: message_id.clone(),
            field,
        })
}

fn consumer_group_from_value_map(
    topic: &str,
    values: HashMap<String, RedisValue>,
) -> Result<ConsumerGroupInfo> {
    let name = value_as_string(values.get("name")).ok_or_else(|| BusError::TopicInfoFailed {
        topic: topic.to_owned(),
        reason: "xinfo_groups missing name field".to_owned(),
    })?;
    let consumers =
        value_as_u64(values.get("consumers")).ok_or_else(|| BusError::TopicInfoFailed {
            topic: topic.to_owned(),
            reason: format!("xinfo_groups missing consumers field for group {name}"),
        })?;
    let pending = value_as_u64(values.get("pending")).ok_or_else(|| BusError::TopicInfoFailed {
        topic: topic.to_owned(),
        reason: "xinfo_groups missing pending field".to_owned(),
    })?;
    let last_delivered = values
        .get("last-delivered-id")
        .and_then(|value| value_as_string(Some(value)))
        .map(MessageId);

    Ok(ConsumerGroupInfo {
        name,
        consumers: u32::try_from(consumers).map_err(|error| BusError::TopicInfoFailed {
            topic: topic.to_owned(),
            reason: format!("consumer count exceeds u32 range: {error}"),
        })?,
        pending,
        last_delivered,
    })
}

fn value_as_string(value: Option<&RedisValue>) -> Option<String> {
    match value? {
        RedisValue::String(inner) => Some(inner.to_string()),
        RedisValue::Bytes(inner) => String::from_utf8(inner.to_vec()).ok(),
        RedisValue::Integer(inner) => Some(inner.to_string()),
        RedisValue::Double(inner) => Some(inner.to_string()),
        RedisValue::Boolean(inner) => Some(inner.to_string()),
        _ => None,
    }
}

fn value_as_u64(value: Option<&RedisValue>) -> Option<u64> {
    match value? {
        RedisValue::Integer(inner) => u64::try_from(*inner).ok(),
        RedisValue::String(inner) => inner.parse().ok(),
        RedisValue::Bytes(inner) => std::str::from_utf8(inner).ok()?.parse().ok(),
        _ => None,
    }
}

fn encode_hex(payload: &[u8]) -> String {
    payload.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(topic: &str, message_id: &MessageId, encoded: &str) -> Result<Vec<u8>> {
    let mut chunks = encoded.as_bytes().chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return Err(BusError::InvalidMessage {
            topic: topic.to_owned(),
            message_id: message_id.clone(),
            reason: "hex payload had odd length".to_owned(),
        });
    }

    let mut decoded = Vec::with_capacity(encoded.len() / 2);
    for chunk in &mut chunks {
        let pair = std::str::from_utf8(chunk).map_err(|error| BusError::InvalidMessage {
            topic: topic.to_owned(),
            message_id: message_id.clone(),
            reason: format!("hex payload was not valid UTF-8: {error}"),
        })?;
        let byte = u8::from_str_radix(pair, 16).map_err(|error| BusError::InvalidMessage {
            topic: topic.to_owned(),
            message_id: message_id.clone(),
            reason: format!("hex payload contained invalid byte {pair}: {error}"),
        })?;
        decoded.push(byte);
    }

    Ok(decoded)
}

fn map_group_or_ack_error(
    topic: &str,
    group: &str,
    message_id: &MessageId,
    error: RedisError,
) -> BusError {
    if is_no_group_error(&error) {
        BusError::GroupNotFound {
            topic: topic.to_owned(),
            group: group.to_owned(),
        }
    } else {
        BusError::AckFailed {
            topic: topic.to_owned(),
            group: group.to_owned(),
            message_id: message_id.clone(),
            reason: error.to_string(),
        }
    }
}

fn map_group_or_pending_error(topic: &str, group: &str, error: RedisError) -> BusError {
    if is_no_group_error(&error) {
        BusError::GroupNotFound {
            topic: topic.to_owned(),
            group: group.to_owned(),
        }
    } else {
        BusError::PendingFailed {
            topic: topic.to_owned(),
            group: group.to_owned(),
            reason: error.to_string(),
        }
    }
}

fn map_topic_info_error(topic: &str, error: RedisError) -> BusError {
    if is_no_such_key_error(&error) {
        BusError::TopicNotFound(topic.to_owned())
    } else {
        BusError::TopicInfoFailed {
            topic: topic.to_owned(),
            reason: error.to_string(),
        }
    }
}

fn is_busy_group_error(error: &RedisError) -> bool {
    error.to_string().contains("BUSYGROUP")
}

fn is_no_group_error(error: &RedisError) -> bool {
    error.to_string().contains("NOGROUP")
}

fn is_no_such_key_error(error: &RedisError) -> bool {
    error.to_string().contains("no such key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_entry_round_trips_binary_payload() {
        let payload = vec![0x00, 0x41, 0x7f, 0xff];
        let timestamp = Utc::now().timestamp_millis().to_string();
        let fields = HashMap::from([
            (PAYLOAD_FIELD.to_owned(), encode_hex(&payload)),
            (PUBLISHED_AT_FIELD.to_owned(), timestamp.clone()),
        ]);

        let message =
            stream_entry_to_message("fills", String::from("1740000000000-0"), fields).unwrap();

        assert_eq!(message.payload, payload);
        assert_eq!(
            message.published_at.timestamp_millis().to_string(),
            timestamp
        );
    }

    #[test]
    fn stream_entry_rejects_odd_hex_payloads() {
        let fields = HashMap::from([
            (PAYLOAD_FIELD.to_owned(), String::from("abc")),
            (PUBLISHED_AT_FIELD.to_owned(), String::from("1740000000000")),
        ]);

        let error =
            stream_entry_to_message("fills", String::from("1740000000000-0"), fields).unwrap_err();

        assert!(matches!(error, BusError::InvalidMessage { .. }));
    }

    #[test]
    fn stream_entry_requires_published_timestamp() {
        let fields = HashMap::from([(PAYLOAD_FIELD.to_owned(), String::from("00"))]);

        let error =
            stream_entry_to_message("fills", String::from("1740000000000-0"), fields).unwrap_err();

        assert!(matches!(
            error,
            BusError::MissingField {
                field: PUBLISHED_AT_FIELD,
                ..
            }
        ));
    }
}
