use anyhow::{Context, Result};
use bus::{MessageBus, RedisBus};
use fred::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

const REDIS_URL: &str = "redis://127.0.0.1:6379";
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);
static TEST_SUFFIX: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
#[ignore = "requires Redis at redis://127.0.0.1:6379"]
async fn test_pub_sub_round_trip() -> Result<()> {
    let context = TestContext::new("round-trip").await?;
    let bus = RedisBus::connect(REDIS_URL).await?;
    let payload = vec![1, 2, 3];

    bus.publish(&context.topic, &payload)
        .await
        .context("publish round-trip payload")?;

    let mut rx = bus
        .subscribe(&context.topic, &context.group, &context.consumer)
        .await
        .context("subscribe round-trip consumer")?;

    let message = recv_message(&mut rx).await?;
    anyhow::ensure!(
        message.topic == context.topic,
        "unexpected topic {}",
        message.topic
    );
    anyhow::ensure!(
        message.payload == payload,
        "expected payload {:?}, got {:?}",
        payload,
        message.payload
    );

    bus.ack(&context.topic, &context.group, &message.id)
        .await
        .context("ack round-trip message")?;

    let pending = bus
        .pending(&context.topic, &context.group, Duration::ZERO)
        .await
        .context("check pending entries after ack")?;
    anyhow::ensure!(pending.is_empty(), "expected no pending entries after ack");

    context.cleanup().await
}

#[tokio::test]
#[ignore = "requires Redis at redis://127.0.0.1:6379"]
async fn test_consumer_group_redelivery() -> Result<()> {
    let context = TestContext::new("pending").await?;
    let bus = RedisBus::connect(REDIS_URL).await?;
    let payload = vec![4, 5, 6];

    bus.publish(&context.topic, &payload)
        .await
        .context("publish pending payload")?;

    let mut rx = bus
        .subscribe(&context.topic, &context.group, &context.consumer)
        .await
        .context("subscribe pending consumer")?;

    let message = recv_message(&mut rx).await?;
    let pending = bus
        .pending(&context.topic, &context.group, Duration::ZERO)
        .await
        .context("lookup pending entries")?;

    anyhow::ensure!(pending.len() == 1, "expected exactly one pending entry");
    let pending_message = &pending[0];
    anyhow::ensure!(
        pending_message.id == message.id,
        "expected pending id {}, got {}",
        message.id,
        pending_message.id
    );
    anyhow::ensure!(
        pending_message.payload == payload,
        "expected pending payload {:?}, got {:?}",
        payload,
        pending_message.payload
    );

    context.cleanup().await
}

#[tokio::test]
#[ignore = "requires Redis at redis://127.0.0.1:6379"]
async fn test_stuck_message_claim() -> Result<()> {
    let context = TestContext::new("claim").await?;
    let bus = RedisBus::connect(REDIS_URL).await?;
    let payload = vec![7, 8, 9];

    bus.publish(&context.topic, &payload)
        .await
        .context("publish claim payload")?;

    let mut rx = bus
        .subscribe(&context.topic, &context.group, &context.consumer)
        .await
        .context("subscribe original consumer")?;

    let message = recv_message(&mut rx).await?;
    drop(rx);

    let claimed = bus
        .claim(
            &context.topic,
            &context.group,
            &context.recovery_consumer,
            Duration::ZERO,
            &[message.id.clone()],
        )
        .await
        .context("claim stuck message")?;

    anyhow::ensure!(claimed.len() == 1, "expected exactly one claimed entry");
    let claimed_message = &claimed[0];
    anyhow::ensure!(
        claimed_message.id == message.id,
        "expected claimed id {}, got {}",
        message.id,
        claimed_message.id
    );
    anyhow::ensure!(
        claimed_message.payload == payload,
        "expected claimed payload {:?}, got {:?}",
        payload,
        claimed_message.payload
    );

    bus.ack(&context.topic, &context.group, &claimed_message.id)
        .await
        .context("ack claimed message")?;

    let pending = bus
        .pending(&context.topic, &context.group, Duration::ZERO)
        .await
        .context("check pending entries after claim ack")?;
    anyhow::ensure!(
        pending.is_empty(),
        "expected no pending entries after claim ack"
    );

    context.cleanup().await
}

struct TestContext {
    admin: RedisClient,
    topic: String,
    group: String,
    consumer: String,
    recovery_consumer: String,
}

impl TestContext {
    async fn new(case_name: &str) -> Result<Self> {
        let admin = connect_admin_client().await?;
        let suffix = next_suffix();
        let topic = format!("bus-test-{case_name}-stream-{suffix}");
        let group = format!("bus-test-{case_name}-group-{suffix}");
        let consumer = format!("bus-test-{case_name}-consumer-{suffix}");
        let recovery_consumer = format!("bus-test-{case_name}-recovery-{suffix}");

        delete_stream(&admin, &topic).await?;

        Ok(Self {
            admin,
            topic,
            group,
            consumer,
            recovery_consumer,
        })
    }

    async fn cleanup(self) -> Result<()> {
        delete_stream(&self.admin, &self.topic).await?;
        self.admin.quit().await.context("close Redis test client")
    }
}

async fn recv_message(rx: &mut mpsc::Receiver<bus::BusMessage>) -> Result<bus::BusMessage> {
    timeout(RECEIVE_TIMEOUT, rx.recv())
        .await
        .context("timed out waiting for bus message")?
        .context("bus subscription closed before delivering a message")
}

async fn connect_admin_client() -> Result<RedisClient> {
    let config =
        RedisConfig::from_url(REDIS_URL).context("parse Redis URL for integration tests")?;
    let client = Builder::from_config(config)
        .build()
        .context("build Redis test client")?;
    client.init().await.context("connect Redis test client")?;
    Ok(client)
}

async fn delete_stream(client: &RedisClient, topic: &str) -> Result<()> {
    let _: () = client
        .del(topic)
        .await
        .with_context(|| format!("delete Redis stream {topic}"))?;
    Ok(())
}

fn next_suffix() -> u64 {
    TEST_SUFFIX.fetch_add(1, Ordering::Relaxed)
}
