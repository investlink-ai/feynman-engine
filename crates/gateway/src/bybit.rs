use crate::{
    ConnectionState, GatewayError, MarketId, OrderState, OrderSubmission, Result, TimeoutPolicy,
    VenueAdapter, VenueBalance, VenueCapabilities, VenueConnectionHealth, VenueFill, VenueId,
    VenueOpenOrder, VenueOrderAck, VenueOrderId, VenuePosition,
};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, warn};

type HmacSha256 = Hmac<Sha256>;

const BYBIT_LINEAR_CATEGORY: &str = "linear";
const BYBIT_TESTNET_REST_BASE_URL: &str = "https://api-testnet.bybit.com";
const BYBIT_PRODUCTION_REST_BASE_URL: &str = "https://api.bybit.com";
const BYBIT_TESTNET_PRIVATE_WS_URL: &str = "wss://stream-testnet.bybit.com/v5/private";
const BYBIT_PRODUCTION_PRIVATE_WS_URL: &str = "wss://stream.bybit.com/v5/private";
const BYBIT_TESTNET_PUBLIC_LINEAR_WS_URL: &str = "wss://stream-testnet.bybit.com/v5/public/linear";
const BYBIT_PRODUCTION_PUBLIC_LINEAR_WS_URL: &str = "wss://stream.bybit.com/v5/public/linear";

#[derive(Debug, Clone)]
pub struct BybitAuth {
    pub api_key: String,
    pub api_secret: SecretString,
}

#[derive(Debug, Clone)]
pub struct BybitConfig {
    pub venue_id: VenueId,
    pub testnet: bool,
    pub auth: BybitAuth,
    pub timeout_policy: TimeoutPolicy,
    pub rest_timeout: Duration,
    pub recv_window_ms: u64,
    pub fill_channel_capacity: usize,
    pub orderbook_markets: Vec<MarketId>,
    pub enable_private_stream: bool,
    pub enable_public_stream: bool,
}

impl BybitConfig {
    pub const DEFAULT_FILL_CHANNEL_CAPACITY: usize = 64;
    pub const DEFAULT_RECV_WINDOW_MS: u64 = 5_000;
    pub const DEFAULT_REST_TIMEOUT: Duration = Duration::from_secs(5);

    #[must_use]
    pub fn new(testnet: bool, auth: BybitAuth) -> Self {
        Self {
            venue_id: VenueId("bybit".into()),
            testnet,
            auth,
            timeout_policy: TimeoutPolicy {
                submit_order: Duration::from_secs(5),
                cancel_order: Duration::from_secs(5),
                amend_order: Duration::from_secs(5),
                query_positions: Duration::from_secs(5),
                query_balance: Duration::from_secs(5),
                query_orders: Duration::from_secs(5),
                fill_watchdog: Duration::from_secs(60),
            },
            rest_timeout: Self::DEFAULT_REST_TIMEOUT,
            recv_window_ms: Self::DEFAULT_RECV_WINDOW_MS,
            fill_channel_capacity: Self::DEFAULT_FILL_CHANNEL_CAPACITY,
            orderbook_markets: Vec::new(),
            enable_private_stream: true,
            enable_public_stream: true,
        }
    }

    #[must_use]
    pub fn rest_base_url(&self) -> &'static str {
        if self.testnet {
            BYBIT_TESTNET_REST_BASE_URL
        } else {
            BYBIT_PRODUCTION_REST_BASE_URL
        }
    }

    #[must_use]
    pub fn private_ws_url(&self) -> &'static str {
        if self.testnet {
            BYBIT_TESTNET_PRIVATE_WS_URL
        } else {
            BYBIT_PRODUCTION_PRIVATE_WS_URL
        }
    }

    #[must_use]
    pub fn public_linear_ws_url(&self) -> &'static str {
        if self.testnet {
            BYBIT_TESTNET_PUBLIC_LINEAR_WS_URL
        } else {
            BYBIT_PRODUCTION_PUBLIC_LINEAR_WS_URL
        }
    }
}

#[derive(Debug, Default)]
struct BybitAdapterState {
    idempotency_cache: HashMap<crate::ClientOrderId, VenueOrderAck>,
    venue_order_markets: HashMap<VenueOrderId, MarketId>,
    latest_orderbooks: HashMap<MarketId, types::OrderbookSnapshot>,
}

#[derive(Debug)]
struct BybitRuntime {
    tasks: Vec<JoinHandle<()>>,
}

#[derive(Debug)]
struct TokenBucketRateLimiter {
    capacity: u32,
    refill_rate_per_sec: u32,
    state: Mutex<TokenBucketState>,
}

#[derive(Debug)]
struct TokenBucketState {
    available_tokens: u32,
    last_refill: Instant,
}

impl TokenBucketRateLimiter {
    fn new(capacity: u32, refill_rate_per_sec: u32) -> Self {
        Self {
            capacity,
            refill_rate_per_sec,
            state: Mutex::new(TokenBucketState {
                available_tokens: capacity,
                last_refill: Instant::now(),
            }),
        }
    }

    async fn acquire(&self) {
        loop {
            let sleep_for = {
                let mut state = self.state.lock().await;
                let elapsed = state.last_refill.elapsed();
                let replenished =
                    (elapsed.as_secs_f64() * f64::from(self.refill_rate_per_sec)).floor() as u32;
                if replenished > 0 {
                    state.available_tokens =
                        (state.available_tokens + replenished).min(self.capacity);
                    state.last_refill = Instant::now();
                }

                if state.available_tokens > 0 {
                    state.available_tokens -= 1;
                    None
                } else {
                    Some(Duration::from_millis(100))
                }
            };

            if let Some(duration) = sleep_for {
                tokio::time::sleep(duration).await;
            } else {
                return;
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum BybitRestError {
    #[error("duplicate client order id")]
    DuplicateClientOrderId,
    #[error("order not found")]
    OrderNotFound,
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("venue rejected request: {0}")]
    VenueRejected(String),
    #[error(transparent)]
    Transport(#[from] anyhow::Error),
}

impl From<BybitRestError> for GatewayError {
    fn from(value: BybitRestError) -> Self {
        match value {
            BybitRestError::DuplicateClientOrderId => GatewayError::VenueRejected {
                reason: "duplicate client order id rejected by bybit".to_owned(),
            },
            BybitRestError::OrderNotFound => GatewayError::OrderNotFound("bybit order".to_owned()),
            BybitRestError::Validation(reason) => GatewayError::Validation(reason),
            BybitRestError::VenueRejected(reason) => GatewayError::VenueRejected { reason },
            BybitRestError::Transport(err) => GatewayError::Internal(err),
        }
    }
}

#[async_trait]
trait BybitRestClient: Send + Sync {
    async fn create_order(
        &self,
        request: BybitCreateOrderRequest,
    ) -> std::result::Result<BybitCreateOrderResult, BybitRestError>;

    async fn get_order_by_link_id(
        &self,
        market_id: &MarketId,
        client_order_id: &crate::ClientOrderId,
    ) -> std::result::Result<Option<BybitOrderRecord>, BybitRestError>;

    async fn cancel_order(
        &self,
        market_id: &MarketId,
        venue_order_id: &VenueOrderId,
    ) -> std::result::Result<(), BybitRestError>;

    async fn get_positions(&self) -> std::result::Result<Vec<BybitPositionRecord>, BybitRestError>;
    async fn get_open_orders(&self) -> std::result::Result<Vec<BybitOrderRecord>, BybitRestError>;
    async fn get_balance(&self) -> std::result::Result<BybitBalanceRecord, BybitRestError>;
}

struct ReqwestBybitRestClient {
    config: BybitConfig,
    http: reqwest::Client,
}

impl ReqwestBybitRestClient {
    fn new(config: BybitConfig) -> std::result::Result<Self, BybitRestError> {
        let http = reqwest::Client::builder()
            .timeout(config.rest_timeout)
            .build()
            .map_err(|err| BybitRestError::Transport(anyhow::anyhow!(err)))?;
        Ok(Self { config, http })
    }

    async fn private_post<TReq, TResp>(
        &self,
        path: &str,
        request: &TReq,
    ) -> std::result::Result<TResp, BybitRestError>
    where
        TReq: Serialize + ?Sized,
        TResp: for<'de> Deserialize<'de>,
    {
        let body = serde_json::to_string(request)
            .map_err(|err| BybitRestError::Transport(anyhow::anyhow!(err)))?;
        let timestamp = Utc::now().timestamp_millis().to_string();
        let signature = sign_rest_payload(
            &self.config.auth.api_secret,
            &timestamp,
            &self.config.auth.api_key,
            self.config.recv_window_ms,
            &body,
        )?;

        let response = self
            .http
            .post(format!("{}{}", self.config.rest_base_url(), path))
            .header("X-BAPI-API-KEY", &self.config.auth.api_key)
            .header("X-BAPI-SIGN", signature)
            .header("X-BAPI-TIMESTAMP", timestamp)
            .header("X-BAPI-RECV-WINDOW", self.config.recv_window_ms.to_string())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|err| BybitRestError::Transport(anyhow::anyhow!(err)))?;

        let envelope: BybitEnvelope<TResp> = response
            .json()
            .await
            .map_err(|err| BybitRestError::Transport(anyhow::anyhow!(err)))?;

        if envelope.ret_code != 0 {
            return Err(classify_bybit_error(envelope.ret_code, &envelope.ret_msg));
        }

        Ok(envelope.result)
    }

    async fn private_get<TResp>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> std::result::Result<TResp, BybitRestError>
    where
        TResp: for<'de> Deserialize<'de>,
    {
        let query_string = query
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        let timestamp = Utc::now().timestamp_millis().to_string();
        let signature = sign_rest_payload(
            &self.config.auth.api_secret,
            &timestamp,
            &self.config.auth.api_key,
            self.config.recv_window_ms,
            &query_string,
        )?;

        let request = self
            .http
            .get(format!("{}{}", self.config.rest_base_url(), path))
            .header("X-BAPI-API-KEY", &self.config.auth.api_key)
            .header("X-BAPI-SIGN", signature)
            .header("X-BAPI-TIMESTAMP", timestamp)
            .header("X-BAPI-RECV-WINDOW", self.config.recv_window_ms.to_string())
            .query(query);

        let response = request
            .send()
            .await
            .map_err(|err| BybitRestError::Transport(anyhow::anyhow!(err)))?;

        let envelope: BybitEnvelope<TResp> = response
            .json()
            .await
            .map_err(|err| BybitRestError::Transport(anyhow::anyhow!(err)))?;

        if envelope.ret_code != 0 {
            return Err(classify_bybit_error(envelope.ret_code, &envelope.ret_msg));
        }

        Ok(envelope.result)
    }
}

#[async_trait]
impl BybitRestClient for ReqwestBybitRestClient {
    async fn create_order(
        &self,
        request: BybitCreateOrderRequest,
    ) -> std::result::Result<BybitCreateOrderResult, BybitRestError> {
        self.private_post("/v5/order/create", &request).await
    }

    async fn get_order_by_link_id(
        &self,
        market_id: &MarketId,
        client_order_id: &crate::ClientOrderId,
    ) -> std::result::Result<Option<BybitOrderRecord>, BybitRestError> {
        let result: BybitListResponse<BybitOrderRecord> = self
            .private_get(
                "/v5/order/realtime",
                &[
                    ("category", BYBIT_LINEAR_CATEGORY.to_owned()),
                    ("symbol", market_id.to_string()),
                    ("orderLinkId", client_order_id.to_string()),
                    ("openOnly", "0".to_owned()),
                    ("limit", "1".to_owned()),
                ],
            )
            .await?;
        Ok(result.list.into_iter().next())
    }

    async fn cancel_order(
        &self,
        market_id: &MarketId,
        venue_order_id: &VenueOrderId,
    ) -> std::result::Result<(), BybitRestError> {
        let _response: BybitCancelOrderResult = self
            .private_post(
                "/v5/order/cancel",
                &BybitCancelOrderRequest {
                    category: BYBIT_LINEAR_CATEGORY.to_owned(),
                    symbol: market_id.to_string(),
                    order_id: venue_order_id.to_string(),
                    order_link_id: None,
                },
            )
            .await?;
        Ok(())
    }

    async fn get_positions(&self) -> std::result::Result<Vec<BybitPositionRecord>, BybitRestError> {
        let result: BybitListResponse<BybitPositionRecord> = self
            .private_get(
                "/v5/position/list",
                &[
                    ("category", BYBIT_LINEAR_CATEGORY.to_owned()),
                    ("settleCoin", "USDT".to_owned()),
                ],
            )
            .await?;
        Ok(result.list)
    }

    async fn get_open_orders(&self) -> std::result::Result<Vec<BybitOrderRecord>, BybitRestError> {
        let result: BybitListResponse<BybitOrderRecord> = self
            .private_get(
                "/v5/order/realtime",
                &[
                    ("category", BYBIT_LINEAR_CATEGORY.to_owned()),
                    ("openOnly", "0".to_owned()),
                    ("limit", "50".to_owned()),
                ],
            )
            .await?;
        Ok(result.list)
    }

    async fn get_balance(&self) -> std::result::Result<BybitBalanceRecord, BybitRestError> {
        let result: BybitWalletBalanceResponse = self
            .private_get(
                "/v5/account/wallet-balance",
                &[("accountType", "UNIFIED".to_owned())],
            )
            .await?;

        result.list.into_iter().next().ok_or_else(|| {
            BybitRestError::Validation("bybit wallet balance returned no accounts".to_owned())
        })
    }
}

pub struct BybitAdapter {
    config: BybitConfig,
    connection_health: VenueConnectionHealth,
    capabilities: VenueCapabilities,
    rest_client: Arc<dyn BybitRestClient>,
    rate_limiter: Arc<TokenBucketRateLimiter>,
    state: Arc<Mutex<BybitAdapterState>>,
    fill_subscribers: Arc<Mutex<Vec<mpsc::Sender<VenueFill>>>>,
    private_stream_ready: Arc<Mutex<bool>>,
    runtime: Option<BybitRuntime>,
}

impl BybitAdapter {
    pub fn new(config: BybitConfig) -> Result<Self> {
        let rest_client =
            Arc::new(ReqwestBybitRestClient::new(config.clone()).map_err(GatewayError::from)?);
        Ok(Self::with_rest_client(config, rest_client))
    }

    #[must_use]
    fn with_rest_client(config: BybitConfig, rest_client: Arc<dyn BybitRestClient>) -> Self {
        let venue_id = config.venue_id.clone();
        Self {
            config,
            connection_health: VenueConnectionHealth::new(venue_id.clone()),
            capabilities: VenueCapabilities {
                venue_id,
                supports_market_orders: true,
                supports_limit_orders: true,
                supports_stop_market: true,
                supports_stop_limit: true,
                supports_trailing_stop: false,
                supports_oco: false,
                supports_amendment: false,
                supports_reduce_only: true,
                supports_post_only: true,
                max_batch_size: Some(1),
            },
            rest_client,
            rate_limiter: Arc::new(TokenBucketRateLimiter::new(10, 10)),
            state: Arc::new(Mutex::new(BybitAdapterState::default())),
            fill_subscribers: Arc::new(Mutex::new(Vec::new())),
            private_stream_ready: Arc::new(Mutex::new(false)),
            runtime: None,
        }
    }

    pub async fn latest_orderbook(&self, market_id: &MarketId) -> Option<types::OrderbookSnapshot> {
        let state = self.state.lock().await;
        state.latest_orderbooks.get(market_id).cloned()
    }

    fn validate_submission(&self, submission: &OrderSubmission) -> Result<()> {
        if submission.venue_id != self.config.venue_id {
            return Err(GatewayError::Validation(format!(
                "bybit adapter {} cannot accept order for venue {}",
                self.config.venue_id, submission.venue_id
            )));
        }

        match submission.order_type {
            types::OrderType::Market
            | types::OrderType::Limit
            | types::OrderType::StopMarket
            | types::OrderType::StopLimit => {}
            unsupported => {
                return Err(GatewayError::Unsupported(format!(
                    "bybit adapter does not support order type {unsupported}"
                )));
            }
        }

        match submission.time_in_force {
            types::TimeInForce::GTC | types::TimeInForce::IOC | types::TimeInForce::FOK => {}
            types::TimeInForce::GTD { .. } => {
                return Err(GatewayError::Unsupported(
                    "bybit adapter does not support GTD orders".to_owned(),
                ));
            }
        }

        if matches!(
            submission.order_type,
            types::OrderType::Limit | types::OrderType::StopLimit
        ) && submission.price.is_none()
        {
            return Err(GatewayError::Validation(
                "limit-style bybit orders require price".to_owned(),
            ));
        }

        if matches!(
            submission.order_type,
            types::OrderType::StopMarket | types::OrderType::StopLimit
        ) && submission.trigger_price.is_none()
        {
            return Err(GatewayError::Validation(
                "conditional bybit orders require trigger_price".to_owned(),
            ));
        }

        Ok(())
    }

    async fn market_for_venue_order_id(&self, venue_order_id: &VenueOrderId) -> Result<MarketId> {
        {
            let state = self.state.lock().await;
            if let Some(market_id) = state.venue_order_markets.get(venue_order_id) {
                return Ok(market_id.clone());
            }
        }

        let open_orders = self.query_open_orders().await?;
        open_orders
            .into_iter()
            .find(|order| order.venue_order_id == *venue_order_id)
            .map(|order| MarketId(order.instrument_id.to_string()))
            .ok_or_else(|| GatewayError::OrderNotFound(venue_order_id.to_string()))
    }

    #[cfg(test)]
    async fn handle_private_ws_message(&self, text: &str) -> Result<()> {
        match handle_private_text(&self.fill_subscribers, &self.state, text).await? {
            PrivateStreamControl::Authenticated
            | PrivateStreamControl::Subscribed
            | PrivateStreamControl::Ignore => Ok(()),
        }
    }

    #[cfg(test)]
    async fn handle_public_ws_message(&self, text: &str) -> Result<()> {
        handle_public_text(&self.config.venue_id, &self.state, text).await
    }

    async fn spawn_private_stream_task(&self) -> JoinHandle<()> {
        let config = self.config.clone();
        let fill_subscribers = Arc::clone(&self.fill_subscribers);
        let state = Arc::clone(&self.state);
        let private_stream_ready = Arc::clone(&self.private_stream_ready);
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);

            loop {
                {
                    let mut ready = private_stream_ready.lock().await;
                    *ready = false;
                }
                match connect_async(config.private_ws_url()).await {
                    Ok((stream, _response)) => {
                        let (mut write, mut read) = stream.split();
                        if let Err(err) = authenticate_private_stream(&config, &mut write).await {
                            error!(error = %err, "bybit private stream auth failed");
                        } else {
                            let mut stream_ready = false;
                            while !stream_ready {
                                match read.next().await {
                                    Some(Ok(Message::Text(text))) => {
                                        match handle_private_text(&fill_subscribers, &state, &text)
                                            .await
                                        {
                                            Ok(PrivateStreamControl::Authenticated) => {
                                                if let Err(err) =
                                                    subscribe_private_topics(&mut write).await
                                                {
                                                    error!(
                                                        error = %err,
                                                        "bybit private stream subscribe failed"
                                                    );
                                                    break;
                                                }
                                            }
                                            Ok(PrivateStreamControl::Subscribed) => {
                                                let mut ready = private_stream_ready.lock().await;
                                                *ready = true;
                                                stream_ready = true;
                                            }
                                            Ok(PrivateStreamControl::Ignore) => {}
                                            Err(err) => {
                                                error!(
                                                    error = %err,
                                                    "failed to handle bybit private message"
                                                );
                                                break;
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Ping(payload))) => {
                                        if let Err(err) = write.send(Message::Pong(payload)).await {
                                            error!(
                                                error = %err,
                                                "failed to pong bybit private stream"
                                            );
                                            break;
                                        }
                                    }
                                    Some(Ok(Message::Close(_))) | None => break,
                                    Some(Ok(_)) => {}
                                    Some(Err(err)) => {
                                        error!(error = %err, "bybit private stream error");
                                        break;
                                    }
                                }
                            }

                            if stream_ready {
                                backoff = Duration::from_secs(1);
                                let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
                                loop {
                                    tokio::select! {
                                        _ = heartbeat.tick() => {
                                            if let Err(err) = write.send(Message::Text(r#"{"op":"ping"}"#.to_owned())).await {
                                                error!(error = %err, "bybit private stream ping failed");
                                                break;
                                            }
                                        }
                                        message = read.next() => {
                                            match message {
                                                Some(Ok(Message::Text(text))) => {
                                                    if let Err(err) = handle_private_text(
                                                        &fill_subscribers,
                                                        &state,
                                                        &text,
                                                    )
                                                    .await
                                                    {
                                                        error!(error = %err, "failed to handle bybit private message");
                                                    }
                                                }
                                                Some(Ok(Message::Ping(payload))) => {
                                                    if let Err(err) = write.send(Message::Pong(payload)).await {
                                                        error!(error = %err, "failed to pong bybit private stream");
                                                        break;
                                                    }
                                                }
                                                Some(Ok(Message::Close(_))) | None => break,
                                                Some(Ok(_)) => {}
                                                Some(Err(err)) => {
                                                    error!(error = %err, "bybit private stream error");
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => error!(error = %err, "bybit private stream connection failed"),
                }

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        })
    }

    async fn spawn_public_stream_task(&self) -> Option<JoinHandle<()>> {
        if self.config.orderbook_markets.is_empty() {
            return None;
        }

        let config = self.config.clone();
        let state = Arc::clone(&self.state);
        Some(tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);

            loop {
                match connect_async(config.public_linear_ws_url()).await {
                    Ok((stream, _response)) => {
                        let (mut write, mut read) = stream.split();
                        if let Err(err) =
                            subscribe_public_topics(&config.orderbook_markets, &mut write).await
                        {
                            error!(error = %err, "bybit public stream subscribe failed");
                        } else {
                            backoff = Duration::from_secs(1);
                            let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
                            loop {
                                tokio::select! {
                                    _ = heartbeat.tick() => {
                                        if let Err(err) = write.send(Message::Text(r#"{"op":"ping"}"#.to_owned())).await {
                                            error!(error = %err, "bybit public stream ping failed");
                                            break;
                                        }
                                    }
                                    message = read.next() => {
                                        match message {
                                            Some(Ok(Message::Text(text))) => {
                                                if let Err(err) = handle_public_text(&config.venue_id, &state, &text).await {
                                                    error!(error = %err, "failed to handle bybit public message");
                                                }
                                            }
                                            Some(Ok(Message::Ping(payload))) => {
                                                if let Err(err) = write.send(Message::Pong(payload)).await {
                                                    error!(error = %err, "failed to pong bybit public stream");
                                                    break;
                                                }
                                            }
                                            Some(Ok(Message::Close(_))) | None => break,
                                            Some(Ok(_)) => {}
                                            Some(Err(err)) => {
                                                error!(error = %err, "bybit public stream error");
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => error!(error = %err, "bybit public stream connection failed"),
                }

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }))
    }
}

#[async_trait]
impl VenueAdapter for BybitAdapter {
    fn venue_id(&self) -> &VenueId {
        &self.config.venue_id
    }

    fn connection_health(&self) -> &VenueConnectionHealth {
        &self.connection_health
    }

    fn timeout_policy(&self) -> &TimeoutPolicy {
        &self.config.timeout_policy
    }

    async fn submit_order(&self, submission: OrderSubmission) -> Result<VenueOrderAck> {
        self.validate_submission(&submission)?;

        {
            let state = self.state.lock().await;
            if let Some(existing) = state.idempotency_cache.get(&submission.client_order_id) {
                return Ok(existing.clone());
            }
        }

        if submission.dry_run {
            warn!(
                client_order_id = %submission.client_order_id,
                "bybit dry_run enabled — returning synthetic venue order id"
            );
            let ack = VenueOrderAck {
                venue_order_id: VenueOrderId(format!(
                    "bybit-dryrun-{}",
                    submission.client_order_id
                )),
                client_order_id: submission.client_order_id.clone(),
                accepted_at: Utc::now(),
            };
            let mut state = self.state.lock().await;
            state
                .venue_order_markets
                .insert(ack.venue_order_id.clone(), submission.market_id.clone());
            state
                .idempotency_cache
                .insert(submission.client_order_id.clone(), ack.clone());
            return Ok(ack);
        }

        if !self.connection_health.is_submittable() {
            return Err(GatewayError::VenueNotConnected {
                venue_id: self.config.venue_id.to_string(),
                state: format!("{:?}", self.connection_health.state),
            });
        }

        if self.config.enable_private_stream {
            let ready = self.private_stream_ready.lock().await;
            if !*ready {
                return Err(GatewayError::VenueNotConnected {
                    venue_id: self.config.venue_id.to_string(),
                    state: "private stream not ready".to_owned(),
                });
            }
        }

        self.rate_limiter.acquire().await;
        let request = translate_submission(&submission)?;
        let result = match self.rest_client.create_order(request).await {
            Ok(result) => result,
            Err(BybitRestError::DuplicateClientOrderId) => {
                let existing = self
                    .rest_client
                    .get_order_by_link_id(&submission.market_id, &submission.client_order_id)
                    .await
                    .map_err(GatewayError::from)?
                    .ok_or_else(|| {
                        GatewayError::VenueRejected {
                            reason: "bybit reported duplicate client order id but no existing order was returned".to_owned(),
                        }
                    })?;
                BybitCreateOrderResult {
                    order_id: existing.order_id,
                    _order_link_id: existing
                        .order_link_id
                        .unwrap_or_else(|| submission.client_order_id.to_string()),
                }
            }
            Err(err) => return Err(err.into()),
        };

        let ack = VenueOrderAck {
            venue_order_id: VenueOrderId(result.order_id),
            client_order_id: submission.client_order_id.clone(),
            accepted_at: Utc::now(),
        };
        let mut state = self.state.lock().await;
        state
            .venue_order_markets
            .insert(ack.venue_order_id.clone(), submission.market_id.clone());
        state
            .idempotency_cache
            .insert(submission.client_order_id, ack.clone());
        Ok(ack)
    }

    async fn cancel_order(&self, venue_order_id: &VenueOrderId) -> Result<()> {
        let market_id = self.market_for_venue_order_id(venue_order_id).await?;
        self.rate_limiter.acquire().await;
        self.rest_client
            .cancel_order(&market_id, venue_order_id)
            .await
            .map_err(GatewayError::from)
    }

    async fn amend_order(
        &self,
        _venue_order_id: &VenueOrderId,
        _new_price: Option<Decimal>,
        _new_qty: Option<Decimal>,
    ) -> Result<VenueOrderAck> {
        Err(GatewayError::Unsupported(
            "bybit adapter amend_order is not implemented yet".to_owned(),
        ))
    }

    async fn query_positions(&self) -> Result<Vec<VenuePosition>> {
        let positions = self
            .rest_client
            .get_positions()
            .await
            .map_err(GatewayError::from)?;
        positions
            .into_iter()
            .filter(|position| !position.size.is_zero())
            .map(|position| {
                Ok(VenuePosition {
                    instrument_id: crate::InstrumentId(position.symbol),
                    qty: signed_position_qty(&position.side, position.size)?,
                    avg_entry_price: position.avg_price,
                    unrealized_pnl: position.unrealised_pnl,
                    margin_used: position.position_value,
                })
            })
            .collect()
    }

    async fn query_open_orders(&self) -> Result<Vec<VenueOpenOrder>> {
        let orders = self
            .rest_client
            .get_open_orders()
            .await
            .map_err(GatewayError::from)?;
        let mut state = self.state.lock().await;
        let mut translated = Vec::with_capacity(orders.len());

        for order in orders {
            let venue_order_id = VenueOrderId(order.order_id.clone());
            let market_id = MarketId(order.symbol.clone());
            state
                .venue_order_markets
                .insert(venue_order_id.clone(), market_id);

            translated.push(VenueOpenOrder {
                venue_order_id,
                client_order_id: order.order_link_id.map(crate::ClientOrderId),
                instrument_id: crate::InstrumentId(order.symbol),
                side: parse_bybit_side(&order.side)?,
                qty: order.qty,
                filled_qty: order.cum_exec_qty,
                price: Some(order.price),
                state: parse_bybit_order_state(&order.order_status),
                created_at: millis_to_datetime(order.created_time)?,
            });
        }

        Ok(translated)
    }

    async fn query_balance(&self) -> Result<VenueBalance> {
        let balance = self
            .rest_client
            .get_balance()
            .await
            .map_err(GatewayError::from)?;
        Ok(VenueBalance {
            total_equity: balance.total_equity,
            available_balance: balance.total_available_balance,
            margin_used: balance.total_margin_balance - balance.total_available_balance,
            unrealized_pnl: balance.total_perp_upl,
            as_of: Utc::now(),
        })
    }

    async fn subscribe_fills(&self) -> Result<mpsc::Receiver<VenueFill>> {
        let (tx, rx) = mpsc::channel(self.config.fill_channel_capacity);
        let mut subscribers = self.fill_subscribers.lock().await;
        subscribers.push(tx);
        Ok(rx)
    }

    fn capabilities(&self) -> &VenueCapabilities {
        &self.capabilities
    }

    async fn connect(&mut self) -> Result<()> {
        self.connection_health.state = ConnectionState::Connecting;
        self.connection_health.last_message_at = Some(Utc::now());
        {
            let mut ready = self.private_stream_ready.lock().await;
            *ready = !self.config.enable_private_stream;
        }

        let mut tasks = Vec::new();
        if self.config.enable_private_stream {
            tasks.push(self.spawn_private_stream_task().await);
        }
        if self.config.enable_public_stream {
            if let Some(task) = self.spawn_public_stream_task().await {
                tasks.push(task);
            }
        }

        let now = Utc::now();
        self.connection_health.state = ConnectionState::Connected;
        self.connection_health.last_message_at = Some(now);
        self.connection_health.last_heartbeat_at = Some(now);
        self.connection_health.connected_since = Some(now);
        self.runtime = Some(BybitRuntime { tasks });
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        if let Some(runtime) = self.runtime.take() {
            for task in runtime.tasks {
                task.abort();
            }
        }
        {
            let mut ready = self.private_stream_ready.lock().await;
            *ready = false;
        }

        self.connection_health.state = ConnectionState::Disconnected;
        self.connection_health.connected_since = None;
        Ok(())
    }
}

impl super::sealed::Sealed for BybitAdapter {}

#[derive(Debug, Deserialize)]
struct BybitEnvelope<T> {
    #[serde(rename = "retCode")]
    ret_code: i32,
    #[serde(rename = "retMsg")]
    ret_msg: String,
    result: T,
}

#[derive(Debug, Deserialize)]
struct BybitListResponse<T> {
    list: Vec<T>,
}

#[derive(Debug, Serialize, Clone)]
struct BybitCreateOrderRequest {
    category: String,
    symbol: String,
    side: String,
    #[serde(rename = "orderType")]
    order_type: String,
    qty: String,
    price: Option<String>,
    #[serde(rename = "triggerPrice")]
    trigger_price: Option<String>,
    #[serde(rename = "timeInForce")]
    time_in_force: String,
    #[serde(rename = "orderLinkId")]
    order_link_id: String,
    #[serde(rename = "reduceOnly")]
    reduce_only: bool,
}

#[derive(Debug, Deserialize)]
struct BybitCreateOrderResult {
    #[serde(rename = "orderId")]
    order_id: String,
    #[serde(rename = "orderLinkId")]
    _order_link_id: String,
}

#[derive(Debug, Serialize)]
struct BybitCancelOrderRequest {
    category: String,
    symbol: String,
    #[serde(rename = "orderId")]
    order_id: String,
    #[serde(rename = "orderLinkId")]
    order_link_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BybitCancelOrderResult {
    #[serde(rename = "orderId")]
    _order_id: String,
    #[serde(rename = "orderLinkId")]
    _order_link_id: String,
}

#[derive(Debug, Deserialize, Clone)]
struct BybitOrderRecord {
    #[serde(rename = "orderId")]
    order_id: String,
    #[serde(rename = "orderLinkId")]
    order_link_id: Option<String>,
    symbol: String,
    price: Decimal,
    qty: Decimal,
    side: String,
    #[serde(rename = "orderStatus")]
    order_status: String,
    #[serde(rename = "cumExecQty")]
    cum_exec_qty: Decimal,
    #[serde(rename = "createdTime")]
    created_time: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct BybitPositionRecord {
    symbol: String,
    side: String,
    size: Decimal,
    #[serde(rename = "avgPrice")]
    avg_price: Decimal,
    #[serde(rename = "positionValue")]
    position_value: Decimal,
    #[serde(rename = "unrealisedPnl")]
    unrealised_pnl: Decimal,
}

#[derive(Debug, Deserialize)]
struct BybitWalletBalanceResponse {
    list: Vec<BybitBalanceRecord>,
}

#[derive(Debug, Deserialize)]
struct BybitBalanceRecord {
    #[serde(rename = "totalEquity")]
    total_equity: Decimal,
    #[serde(rename = "totalAvailableBalance")]
    total_available_balance: Decimal,
    #[serde(rename = "totalMarginBalance")]
    total_margin_balance: Decimal,
    #[serde(rename = "totalPerpUPL")]
    total_perp_upl: Decimal,
}

#[derive(Debug, Deserialize)]
struct BybitWsEnvelope<T> {
    #[serde(rename = "topic")]
    _topic: String,
    data: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct BybitWsControlEnvelope {
    topic: Option<String>,
    op: Option<String>,
    success: Option<bool>,
    #[serde(rename = "ret_msg", alias = "retMsg")]
    ret_msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BybitExecutionRecord {
    symbol: String,
    #[serde(rename = "orderId")]
    order_id: String,
    #[serde(rename = "orderLinkId")]
    order_link_id: String,
    side: String,
    #[serde(rename = "execQty")]
    exec_qty: Decimal,
    #[serde(rename = "execPrice")]
    exec_price: Decimal,
    #[serde(rename = "execFee")]
    exec_fee: Decimal,
    #[serde(rename = "execTime")]
    exec_time: i64,
    #[serde(rename = "isMaker")]
    is_maker: bool,
}

#[derive(Debug, Deserialize)]
struct BybitOrderbookData {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "b")]
    bids: Vec<(String, String)>,
    #[serde(rename = "a")]
    asks: Vec<(String, String)>,
    ts: i64,
}

enum PrivateStreamControl {
    Authenticated,
    Subscribed,
    Ignore,
}

fn translate_submission(submission: &OrderSubmission) -> Result<BybitCreateOrderRequest> {
    let order_type = match submission.order_type {
        types::OrderType::Market | types::OrderType::StopMarket => "Market",
        types::OrderType::Limit | types::OrderType::StopLimit => "Limit",
        unsupported => {
            return Err(GatewayError::Unsupported(format!(
                "bybit adapter does not support order type {unsupported}"
            )));
        }
    };
    let time_in_force = if submission.post_only {
        "PostOnly".to_owned()
    } else {
        match submission.time_in_force {
            types::TimeInForce::GTC => "GTC".to_owned(),
            types::TimeInForce::IOC => "IOC".to_owned(),
            types::TimeInForce::FOK => "FOK".to_owned(),
            types::TimeInForce::GTD { .. } => {
                return Err(GatewayError::Unsupported(
                    "bybit adapter does not support GTD orders".to_owned(),
                ));
            }
        }
    };

    Ok(BybitCreateOrderRequest {
        category: BYBIT_LINEAR_CATEGORY.to_owned(),
        symbol: submission.market_id.to_string(),
        side: bybit_side(submission.side).to_owned(),
        order_type: order_type.to_owned(),
        qty: submission.qty.normalize().to_string(),
        price: submission.price.map(|value| value.normalize().to_string()),
        trigger_price: submission
            .trigger_price
            .map(|value| value.normalize().to_string()),
        time_in_force,
        order_link_id: submission.client_order_id.to_string(),
        reduce_only: submission.reduce_only,
    })
}

fn bybit_side(side: types::Side) -> &'static str {
    match side {
        types::Side::Buy => "Buy",
        types::Side::Sell => "Sell",
    }
}

fn parse_bybit_side(side: &str) -> Result<types::Side> {
    match side {
        "Buy" => Ok(types::Side::Buy),
        "Sell" => Ok(types::Side::Sell),
        other => Err(GatewayError::Validation(format!(
            "unknown bybit side {other}"
        ))),
    }
}

fn signed_position_qty(side: &str, size: Decimal) -> Result<Decimal> {
    match side {
        "Buy" => Ok(size),
        "Sell" => Ok(-size),
        "" => Ok(Decimal::ZERO),
        other => Err(GatewayError::Validation(format!(
            "unknown bybit position side {other}"
        ))),
    }
}

fn parse_bybit_order_state(status: &str) -> OrderState {
    match status {
        "New" | "Untriggered" => OrderState::Accepted,
        "PartiallyFilled" => OrderState::PartiallyFilled,
        "Filled" => OrderState::Filled,
        "Cancelled" | "PartiallyFilledCanceled" | "Deactivated" => OrderState::Cancelled,
        "Rejected" => OrderState::Rejected,
        _ => OrderState::Submitted,
    }
}

fn parse_price_levels(levels: &[(String, String)]) -> Result<Vec<types::PriceLevel>> {
    levels
        .iter()
        .map(|(price, qty)| {
            Ok(types::PriceLevel {
                price: parse_decimal(price)?,
                qty: parse_decimal(qty)?,
            })
        })
        .collect()
}

fn parse_decimal(value: &str) -> Result<Decimal> {
    value
        .parse()
        .map_err(|err| GatewayError::Validation(format!("invalid bybit decimal {value}: {err}")))
}

fn millis_to_datetime(value: i64) -> Result<DateTime<Utc>> {
    Utc.timestamp_millis_opt(value)
        .single()
        .ok_or_else(|| GatewayError::Validation(format!("invalid millisecond timestamp {value}")))
}

fn sign_rest_payload(
    secret: &SecretString,
    timestamp: &str,
    api_key: &str,
    recv_window_ms: u64,
    payload: &str,
) -> std::result::Result<String, BybitRestError> {
    let signature_payload = format!("{timestamp}{api_key}{recv_window_ms}{payload}");
    sign_hmac(secret, &signature_payload)
}

fn sign_hmac(secret: &SecretString, payload: &str) -> std::result::Result<String, BybitRestError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|err| BybitRestError::Transport(anyhow::anyhow!("invalid hmac key: {err}")))?;
    mac.update(payload.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn classify_bybit_error(code: i32, message: &str) -> BybitRestError {
    match code {
        10001 => BybitRestError::Validation("bybit rejected the request parameters".to_owned()),
        10014 | 110072 => BybitRestError::DuplicateClientOrderId,
        110001 => BybitRestError::OrderNotFound,
        110003 => BybitRestError::Validation("bybit rejected the order price".to_owned()),
        10006 => BybitRestError::VenueRejected("bybit rate limit exceeded".to_owned()),
        _ if message.to_ascii_lowercase().contains("duplicate") => {
            BybitRestError::DuplicateClientOrderId
        }
        _ => BybitRestError::VenueRejected(format!("bybit api error {code}: request was rejected")),
    }
}

async fn authenticate_private_stream<S>(
    config: &BybitConfig,
    write: &mut S,
) -> std::result::Result<(), anyhow::Error>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let expires = Utc::now().timestamp_millis() + config.recv_window_ms as i64;
    let signature = sign_hmac(&config.auth.api_secret, &format!("GET/realtime{expires}"))
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let auth = serde_json::json!({
        "op": "auth",
        "args": [config.auth.api_key, expires, signature],
    });
    write.send(Message::Text(auth.to_string())).await?;
    Ok(())
}

async fn subscribe_private_topics<S>(write: &mut S) -> std::result::Result<(), anyhow::Error>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let payload = serde_json::json!({
        "op": "subscribe",
        "args": ["execution.linear", "order.linear", "position.linear"],
    });
    write.send(Message::Text(payload.to_string())).await?;
    Ok(())
}

async fn subscribe_public_topics<S>(
    markets: &[MarketId],
    write: &mut S,
) -> std::result::Result<(), anyhow::Error>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let args = markets
        .iter()
        .map(|market| format!("orderbook.1.{market}"))
        .collect::<Vec<_>>();
    let payload = serde_json::json!({
        "op": "subscribe",
        "args": args,
    });
    write.send(Message::Text(payload.to_string())).await?;
    Ok(())
}

async fn handle_private_text(
    fill_subscribers: &Arc<Mutex<Vec<mpsc::Sender<VenueFill>>>>,
    state: &Arc<Mutex<BybitAdapterState>>,
    text: &str,
) -> Result<PrivateStreamControl> {
    let control: BybitWsControlEnvelope = serde_json::from_str(text).map_err(|err| {
        GatewayError::Validation(format!("invalid bybit private ws payload: {err}"))
    })?;

    if let Some(topic) = control.topic.as_deref() {
        if !topic.starts_with("execution") {
            return Ok(PrivateStreamControl::Ignore);
        }

        let message: BybitWsEnvelope<BybitExecutionRecord> =
            serde_json::from_str(text).map_err(|err| {
                GatewayError::Validation(format!("invalid bybit private ws payload: {err}"))
            })?;

        let fills = message
            .data
            .into_iter()
            .filter(|execution| execution.exec_qty != Decimal::ZERO)
            .map(|execution| {
                let venue_order_id = VenueOrderId(execution.order_id);
                let market_id = MarketId(execution.symbol.clone());
                let fill = VenueFill {
                    venue_order_id: venue_order_id.clone(),
                    client_order_id: crate::ClientOrderId(execution.order_link_id),
                    instrument_id: crate::InstrumentId(execution.symbol),
                    side: parse_bybit_side(&execution.side)?,
                    qty: execution.exec_qty,
                    price: execution.exec_price,
                    fee: execution.exec_fee,
                    is_maker: execution.is_maker,
                    filled_at: millis_to_datetime(execution.exec_time)?,
                };
                Ok((venue_order_id, market_id, fill))
            })
            .collect::<Result<Vec<_>>>()?;

        {
            let mut adapter_state = state.lock().await;
            for (venue_order_id, market_id, _) in &fills {
                adapter_state
                    .venue_order_markets
                    .insert(venue_order_id.clone(), market_id.clone());
            }
        }

        let subscribers = {
            let subscribers = fill_subscribers.lock().await;
            subscribers.clone()
        };

        for (_, _, fill) in fills {
            for subscriber in &subscribers {
                let _ = subscriber.send(fill.clone()).await;
            }
        }

        let mut subscribers = fill_subscribers.lock().await;
        subscribers.retain(|sender| !sender.is_closed());
        return Ok(PrivateStreamControl::Ignore);
    }

    match control.op.as_deref() {
        Some("auth") => match control.success {
            Some(true) => Ok(PrivateStreamControl::Authenticated),
            Some(false) => Err(GatewayError::VenueRejected {
                reason: format!(
                    "bybit private stream auth failed: {}",
                    control
                        .ret_msg
                        .unwrap_or_else(|| "request was rejected".to_owned())
                ),
            }),
            None => Ok(PrivateStreamControl::Ignore),
        },
        Some("subscribe") => match control.success {
            Some(true) => Ok(PrivateStreamControl::Subscribed),
            Some(false) => Err(GatewayError::VenueRejected {
                reason: format!(
                    "bybit private stream subscribe failed: {}",
                    control
                        .ret_msg
                        .unwrap_or_else(|| "request was rejected".to_owned())
                ),
            }),
            None => Ok(PrivateStreamControl::Ignore),
        },
        Some("pong") | Some("ping") => Ok(PrivateStreamControl::Ignore),
        _ => Ok(PrivateStreamControl::Ignore),
    }
}

async fn handle_public_text(
    venue_id: &VenueId,
    state: &Arc<Mutex<BybitAdapterState>>,
    text: &str,
) -> Result<()> {
    let control: BybitWsControlEnvelope = serde_json::from_str(text).map_err(|err| {
        GatewayError::Validation(format!("invalid bybit public ws payload: {err}"))
    })?;

    let Some(topic) = control.topic.as_deref() else {
        match control.op.as_deref() {
            Some("subscribe") if control.success == Some(false) => {
                return Err(GatewayError::VenueRejected {
                    reason: format!(
                        "bybit public stream subscribe failed: {}",
                        control
                            .ret_msg
                            .unwrap_or_else(|| "request was rejected".to_owned())
                    ),
                });
            }
            Some("pong") | Some("ping") | Some("subscribe") => return Ok(()),
            _ => return Ok(()),
        }
    };

    if !topic.starts_with("orderbook.") {
        return Ok(());
    }

    let message: BybitWsEnvelope<BybitOrderbookData> =
        serde_json::from_str(text).map_err(|err| {
            GatewayError::Validation(format!("invalid bybit public ws payload: {err}"))
        })?;

    let Some(data) = message.data.into_iter().next() else {
        return Ok(());
    };

    let snapshot = types::OrderbookSnapshot {
        venue_id: venue_id.clone(),
        instrument_id: crate::InstrumentId(data.symbol.clone()),
        market_id: MarketId(data.symbol),
        bids: parse_price_levels(&data.bids)?,
        asks: parse_price_levels(&data.asks)?,
        observed_at: millis_to_datetime(data.ts)?,
        received_at: Utc::now(),
    };

    snapshot
        .validate()
        .map_err(|err| GatewayError::Validation(err.to_string()))?;
    let mut adapter_state = state.lock().await;
    adapter_state
        .latest_orderbooks
        .insert(snapshot.market_id.clone(), snapshot);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{timeout, Duration as TokioDuration};

    #[derive(Default)]
    struct FakeBybitRestClient {
        create_calls: AtomicUsize,
        cancel_calls: AtomicUsize,
        create_result: Mutex<Option<std::result::Result<BybitCreateOrderResult, BybitRestError>>>,
        lookup_result: Mutex<Option<Option<BybitOrderRecord>>>,
        open_orders: Mutex<Vec<BybitOrderRecord>>,
        positions: Mutex<Vec<BybitPositionRecord>>,
        balance: Mutex<Option<BybitBalanceRecord>>,
    }

    #[async_trait]
    impl BybitRestClient for FakeBybitRestClient {
        async fn create_order(
            &self,
            _request: BybitCreateOrderRequest,
        ) -> std::result::Result<BybitCreateOrderResult, BybitRestError> {
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            self.create_result.lock().await.take().unwrap_or_else(|| {
                Ok(BybitCreateOrderResult {
                    order_id: "bybit-order-1".to_owned(),
                    _order_link_id: "client-1".to_owned(),
                })
            })
        }

        async fn get_order_by_link_id(
            &self,
            _market_id: &MarketId,
            _client_order_id: &crate::ClientOrderId,
        ) -> std::result::Result<Option<BybitOrderRecord>, BybitRestError> {
            Ok(self.lookup_result.lock().await.take().unwrap_or(None))
        }

        async fn cancel_order(
            &self,
            _market_id: &MarketId,
            _venue_order_id: &VenueOrderId,
        ) -> std::result::Result<(), BybitRestError> {
            self.cancel_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn get_positions(
            &self,
        ) -> std::result::Result<Vec<BybitPositionRecord>, BybitRestError> {
            Ok(self.positions.lock().await.clone())
        }

        async fn get_open_orders(
            &self,
        ) -> std::result::Result<Vec<BybitOrderRecord>, BybitRestError> {
            Ok(self.open_orders.lock().await.clone())
        }

        async fn get_balance(&self) -> std::result::Result<BybitBalanceRecord, BybitRestError> {
            self.balance
                .lock()
                .await
                .take()
                .ok_or_else(|| BybitRestError::Validation("missing fake balance".to_owned()))
        }
    }

    fn sample_config() -> BybitConfig {
        let mut config = BybitConfig::new(
            true,
            BybitAuth {
                api_key: "test-key".to_owned(),
                api_secret: SecretString::new("test-secret".to_owned()),
            },
        );
        config.enable_private_stream = false;
        config.enable_public_stream = false;
        config
    }

    fn sample_submission(
        client_order_id: &str,
        order_type: types::OrderType,
        tif: types::TimeInForce,
        dry_run: bool,
    ) -> OrderSubmission {
        OrderSubmission {
            order_id: crate::OrderId(format!("ord-{client_order_id}")),
            client_order_id: crate::ClientOrderId(client_order_id.to_owned()),
            agent_id: types::AgentId("athena".into()),
            instrument_id: crate::InstrumentId("BTCUSDT".into()),
            market_id: MarketId("BTCUSDT".into()),
            venue_id: VenueId("bybit".into()),
            side: types::Side::Buy,
            order_type,
            time_in_force: tif,
            qty: Decimal::new(1, 0),
            price: Some(Decimal::new(60_000, 0)),
            trigger_price: Some(Decimal::new(59_500, 0)),
            stop_loss: None,
            take_profit: None,
            post_only: false,
            reduce_only: false,
            dry_run,
            exec_hint: types::ExecHint::default(),
        }
    }

    #[tokio::test]
    async fn bybit_adapter_dry_run_returns_synthetic_ack_without_rest_call() {
        let rest_client = Arc::new(FakeBybitRestClient::default());
        let adapter = BybitAdapter::with_rest_client(sample_config(), rest_client.clone());

        let ack = adapter
            .submit_order(sample_submission(
                "dry-run-1",
                types::OrderType::Limit,
                types::TimeInForce::GTC,
                true,
            ))
            .await
            .unwrap();

        assert!(ack.venue_order_id.to_string().starts_with("bybit-dryrun-"));
        assert_eq!(rest_client.create_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bybit_adapter_rejects_unsupported_order_type() {
        let adapter = BybitAdapter::with_rest_client(
            sample_config(),
            Arc::new(FakeBybitRestClient::default()),
        );
        let err = adapter
            .submit_order(sample_submission(
                "trail-1",
                types::OrderType::TrailingStop,
                types::TimeInForce::GTC,
                false,
            ))
            .await
            .unwrap_err();

        assert!(matches!(err, GatewayError::Unsupported(_)));
    }

    #[tokio::test]
    async fn bybit_adapter_duplicate_client_order_id_uses_cache() {
        let rest_client = Arc::new(FakeBybitRestClient::default());
        let mut adapter = BybitAdapter::with_rest_client(sample_config(), rest_client.clone());
        adapter.connect().await.unwrap();

        let submission = sample_submission(
            "cached-1",
            types::OrderType::Limit,
            types::TimeInForce::GTC,
            false,
        );
        let first_ack = adapter.submit_order(submission.clone()).await.unwrap();
        let second_ack = adapter.submit_order(submission).await.unwrap();

        assert_eq!(first_ack.venue_order_id, second_ack.venue_order_id);
        assert_eq!(rest_client.create_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn bybit_adapter_duplicate_from_venue_falls_back_to_lookup() {
        let rest_client = Arc::new(FakeBybitRestClient::default());
        {
            let mut create_result = rest_client.create_result.lock().await;
            *create_result = Some(Err(BybitRestError::DuplicateClientOrderId));
        }
        {
            let mut lookup_result = rest_client.lookup_result.lock().await;
            *lookup_result = Some(Some(BybitOrderRecord {
                order_id: "existing-order".to_owned(),
                order_link_id: Some("lookup-1".to_owned()),
                symbol: "BTCUSDT".to_owned(),
                price: Decimal::new(60_000, 0),
                qty: Decimal::new(1, 0),
                side: "Buy".to_owned(),
                order_status: "New".to_owned(),
                cum_exec_qty: Decimal::ZERO,
                created_time: 1_711_234_567_890,
            }));
        }

        let mut adapter = BybitAdapter::with_rest_client(sample_config(), rest_client.clone());
        adapter.connect().await.unwrap();
        let ack = adapter
            .submit_order(sample_submission(
                "lookup-1",
                types::OrderType::Limit,
                types::TimeInForce::GTC,
                false,
            ))
            .await
            .unwrap();

        assert_eq!(ack.venue_order_id, VenueOrderId("existing-order".into()));
    }

    #[tokio::test]
    async fn bybit_adapter_private_execution_message_emits_fill() {
        let adapter = BybitAdapter::with_rest_client(
            sample_config(),
            Arc::new(FakeBybitRestClient::default()),
        );
        let mut fills = adapter.subscribe_fills().await.unwrap();
        adapter
            .handle_private_ws_message(
                r#"{
                    "topic":"execution.linear",
                    "data":[
                        {
                            "symbol":"BTCUSDT",
                            "orderId":"order-1",
                            "orderLinkId":"client-1",
                            "side":"Buy",
                            "execQty":"0.5",
                            "execPrice":"60001.5",
                            "execFee":"0.01",
                            "execTime":1711234567890,
                            "isMaker":false
                        }
                    ]
                }"#,
            )
            .await
            .unwrap();

        let fill = timeout(TokioDuration::from_secs(1), fills.recv())
            .await
            .expect("fill should arrive")
            .expect("fill channel should stay open");
        assert_eq!(fill.venue_order_id, VenueOrderId("order-1".into()));
        assert_eq!(
            fill.client_order_id,
            crate::ClientOrderId("client-1".into())
        );
        assert_eq!(fill.qty, Decimal::new(5, 1));
        assert_eq!(fill.price, Decimal::new(600015, 1));
    }

    #[tokio::test]
    async fn bybit_adapter_private_stream_control_messages_do_not_emit_fill() {
        let adapter = BybitAdapter::with_rest_client(
            sample_config(),
            Arc::new(FakeBybitRestClient::default()),
        );
        let mut fills = adapter.subscribe_fills().await.unwrap();

        adapter
            .handle_private_ws_message(r#"{"op":"auth","success":true,"ret_msg":""}"#)
            .await
            .unwrap();
        adapter
            .handle_private_ws_message(r#"{"op":"subscribe","success":true,"ret_msg":""}"#)
            .await
            .unwrap();

        let result = timeout(TokioDuration::from_millis(100), fills.recv()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn bybit_adapter_private_stream_auth_rejection_returns_error() {
        let adapter = BybitAdapter::with_rest_client(
            sample_config(),
            Arc::new(FakeBybitRestClient::default()),
        );

        let err = adapter
            .handle_private_ws_message(r#"{"op":"auth","success":false,"ret_msg":"bad api key"}"#)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            GatewayError::VenueRejected { ref reason }
                if reason.contains("bad api key")
        ));
    }

    #[tokio::test]
    async fn bybit_adapter_public_orderbook_message_updates_snapshot() {
        let adapter = BybitAdapter::with_rest_client(
            sample_config(),
            Arc::new(FakeBybitRestClient::default()),
        );
        adapter
            .handle_public_ws_message(
                r#"{
                    "topic":"orderbook.1.BTCUSDT",
                    "data":[
                        {
                            "s":"BTCUSDT",
                            "b":[["60000.5","1.2"]],
                            "a":[["60001.0","0.9"]],
                            "ts":1711234567890
                        }
                    ]
                }"#,
            )
            .await
            .unwrap();

        let snapshot = adapter
            .latest_orderbook(&MarketId("BTCUSDT".into()))
            .await
            .expect("snapshot");
        assert_eq!(snapshot.bids[0].price, Decimal::new(600005, 1));
        assert_eq!(snapshot.asks[0].qty, Decimal::new(9, 1));
    }

    #[tokio::test]
    async fn bybit_adapter_public_stream_subscribe_ack_is_ignored() {
        let adapter = BybitAdapter::with_rest_client(
            sample_config(),
            Arc::new(FakeBybitRestClient::default()),
        );

        adapter
            .handle_public_ws_message(r#"{"op":"subscribe","success":true,"ret_msg":""}"#)
            .await
            .unwrap();

        assert!(adapter
            .latest_orderbook(&MarketId("BTCUSDT".into()))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn bybit_adapter_duplicate_resting_cancel_uses_cached_market_id() {
        let rest_client = Arc::new(FakeBybitRestClient::default());
        let mut adapter = BybitAdapter::with_rest_client(sample_config(), rest_client.clone());
        adapter.connect().await.unwrap();
        let ack = adapter
            .submit_order(sample_submission(
                "cancel-1",
                types::OrderType::Limit,
                types::TimeInForce::GTC,
                false,
            ))
            .await
            .unwrap();

        adapter.cancel_order(&ack.venue_order_id).await.unwrap();
        assert_eq!(rest_client.cancel_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn bybit_adapter_connect_disconnect_updates_health() {
        let mut adapter = BybitAdapter::with_rest_client(
            sample_config(),
            Arc::new(FakeBybitRestClient::default()),
        );
        adapter.connect().await.unwrap();
        assert!(adapter.connection_health().is_submittable());
        adapter.disconnect().await.unwrap();
        assert!(matches!(
            adapter.connection_health().state,
            ConnectionState::Disconnected
        ));
    }

    #[tokio::test]
    async fn bybit_adapter_requires_private_stream_readiness_for_live_submit() {
        let mut config = sample_config();
        config.enable_private_stream = true;
        let mut adapter =
            BybitAdapter::with_rest_client(config, Arc::new(FakeBybitRestClient::default()));
        adapter.connection_health.state = ConnectionState::Connected;

        let err = adapter
            .submit_order(sample_submission(
                "stream-gate-1",
                types::OrderType::Limit,
                types::TimeInForce::GTC,
                false,
            ))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            GatewayError::VenueNotConnected { ref state, .. }
                if state == "private stream not ready"
        ));
    }
}
