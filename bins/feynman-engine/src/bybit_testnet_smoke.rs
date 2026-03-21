use anyhow::{anyhow, bail, ensure, Context, Result};
use gateway::{
    BybitAdapter, BybitAuth, BybitConfig, ClientOrderId, GatewayError, MarketId, OrderId,
    OrderSubmission, VenueAdapter,
};
use rust_decimal::Decimal;
use secrecy::SecretString;
use std::env;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Instant};
use types::{AgentId, ExecHint, InstrumentId, OrderType, Side, TimeInForce, VenueId};

const DEFAULT_MARKET: &str = "BTCUSDT";
const DEFAULT_QTY: &str = "0.001";
const DEFAULT_LIMIT_OFFSET_BPS: u32 = 500;
const ORDERBOOK_TIMEOUT: Duration = Duration::from_secs(30);
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(45);
const ORDER_STATE_TIMEOUT: Duration = Duration::from_secs(15);
const FILL_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = required_env("BYBIT_API_KEY")?;
    let api_secret = required_env("BYBIT_API_SECRET")?;
    let market = env::var("BYBIT_TESTNET_MARKET").unwrap_or_else(|_| DEFAULT_MARKET.to_owned());
    let qty = parse_decimal_env("BYBIT_TESTNET_QTY", DEFAULT_QTY)?;
    let limit_offset_bps =
        parse_u32_env("BYBIT_TESTNET_LIMIT_OFFSET_BPS", DEFAULT_LIMIT_OFFSET_BPS)?;
    let run_market_fill = parse_bool_env("BYBIT_TESTNET_RUN_MARKET_FILL", false)?;

    let market_id = MarketId(market.clone());
    let mut config = BybitConfig::new(
        true,
        BybitAuth {
            api_key,
            api_secret: SecretString::new(api_secret),
        },
    );
    config.orderbook_markets = vec![market_id.clone()];
    config.enable_private_stream = true;
    config.enable_public_stream = true;

    let mut adapter = BybitAdapter::new(config)?;
    let mut fills = adapter.subscribe_fills().await?;
    adapter.connect().await?;

    let orderbook = wait_for_orderbook(&adapter, &market_id, ORDERBOOK_TIMEOUT).await?;
    let limit_submission = build_resting_limit_submission(
        &market,
        qty,
        limit_offset_bps,
        &orderbook,
        unique_suffix("limit")?,
    )?;

    let limit_ack = submit_with_retry(&adapter, limit_submission.clone(), SUBMIT_TIMEOUT).await?;
    println!(
        "submitted resting testnet limit order {} for {}",
        limit_ack.venue_order_id, limit_submission.client_order_id
    );

    wait_for_order_presence(
        &adapter,
        &limit_ack.venue_order_id,
        true,
        ORDER_STATE_TIMEOUT,
    )
    .await?;
    adapter.cancel_order(&limit_ack.venue_order_id).await?;
    println!("cancel requested for {}", limit_ack.venue_order_id);
    wait_for_order_presence(
        &adapter,
        &limit_ack.venue_order_id,
        false,
        ORDER_STATE_TIMEOUT,
    )
    .await?;
    println!("cancel confirmed for {}", limit_ack.venue_order_id);

    if run_market_fill {
        let market_submission = build_market_submission(&market, qty, unique_suffix("market")?);
        let market_ack =
            submit_with_retry(&adapter, market_submission.clone(), SUBMIT_TIMEOUT).await?;
        println!(
            "submitted market testnet order {} for {}",
            market_ack.venue_order_id, market_submission.client_order_id
        );

        let fill =
            wait_for_fill(&mut fills, &market_submission.client_order_id, FILL_TIMEOUT).await?;
        println!(
            "received fill for {}: qty={} price={} fee={}",
            fill.client_order_id, fill.qty, fill.price, fill.fee
        );
    } else {
        println!(
            "skipping market-fill exercise; set BYBIT_TESTNET_RUN_MARKET_FILL=1 to test websocket fills"
        );
    }

    adapter.disconnect().await?;
    println!("bybit testnet smoke completed");
    Ok(())
}

fn required_env(key: &str) -> Result<String> {
    env::var(key).with_context(|| format!("{key} is required"))
}

fn parse_decimal_env(key: &str, default: &str) -> Result<Decimal> {
    let raw = env::var(key).unwrap_or_else(|_| default.to_owned());
    Decimal::from_str(&raw).with_context(|| format!("{key} must be a decimal, got {raw}"))
}

fn parse_u32_env(key: &str, default: u32) -> Result<u32> {
    match env::var(key) {
        Ok(raw) => raw
            .parse()
            .with_context(|| format!("{key} must be an integer, got {raw}")),
        Err(_) => Ok(default),
    }
}

fn parse_bool_env(key: &str, default: bool) -> Result<bool> {
    match env::var(key) {
        Ok(raw) => match raw.as_str() {
            "1" | "true" | "TRUE" | "yes" | "YES" => Ok(true),
            "0" | "false" | "FALSE" | "no" | "NO" => Ok(false),
            _ => bail!("{key} must be one of 1/0/true/false/yes/no, got {raw}"),
        },
        Err(_) => Ok(default),
    }
}

fn unique_suffix(prefix: &str) -> Result<String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| anyhow!("system clock drifted before UNIX_EPOCH: {err}"))?
        .as_millis();
    Ok(format!("{prefix}-{millis}"))
}

fn build_resting_limit_submission(
    market: &str,
    qty: Decimal,
    limit_offset_bps: u32,
    orderbook: &types::OrderbookSnapshot,
    suffix: String,
) -> Result<OrderSubmission> {
    let best_bid = orderbook
        .bids
        .first()
        .context("orderbook has no bids; cannot price resting limit")?
        .price;
    let offset = Decimal::from(limit_offset_bps) / Decimal::from(10_000_u32);
    ensure!(
        offset > Decimal::ZERO && offset < Decimal::ONE,
        "BYBIT_TESTNET_LIMIT_OFFSET_BPS must be between 1 and 9999, got {}",
        limit_offset_bps
    );

    let scale = best_bid.scale();
    let tick = Decimal::new(1, scale);
    let mut price = (best_bid * (Decimal::ONE - offset)).round_dp(scale);
    if price >= best_bid {
        price = best_bid - tick;
    }
    ensure!(
        price > Decimal::ZERO,
        "computed resting limit price must be positive"
    );

    Ok(OrderSubmission {
        order_id: OrderId(format!("ord-{suffix}")),
        client_order_id: ClientOrderId(format!("smoke-{suffix}")),
        agent_id: AgentId("smoke".to_owned()),
        instrument_id: InstrumentId(market.to_owned()),
        market_id: MarketId(market.to_owned()),
        venue_id: VenueId("bybit".to_owned()),
        side: Side::Buy,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::GTC,
        qty,
        price: Some(price),
        trigger_price: None,
        stop_loss: None,
        take_profit: None,
        post_only: false,
        reduce_only: false,
        dry_run: false,
        exec_hint: ExecHint::default(),
    })
}

fn build_market_submission(market: &str, qty: Decimal, suffix: String) -> OrderSubmission {
    OrderSubmission {
        order_id: OrderId(format!("ord-{suffix}")),
        client_order_id: ClientOrderId(format!("smoke-{suffix}")),
        agent_id: AgentId("smoke".to_owned()),
        instrument_id: InstrumentId(market.to_owned()),
        market_id: MarketId(market.to_owned()),
        venue_id: VenueId("bybit".to_owned()),
        side: Side::Buy,
        order_type: OrderType::Market,
        time_in_force: TimeInForce::IOC,
        qty,
        price: None,
        trigger_price: None,
        stop_loss: None,
        take_profit: None,
        post_only: false,
        reduce_only: false,
        dry_run: false,
        exec_hint: ExecHint::default(),
    }
}

async fn wait_for_orderbook(
    adapter: &BybitAdapter,
    market_id: &MarketId,
    timeout: Duration,
) -> Result<types::OrderbookSnapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(snapshot) = adapter.latest_orderbook(market_id).await {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for public orderbook snapshot for {}",
                market_id
            );
        }
        sleep(Duration::from_millis(250)).await;
    }
}

async fn submit_with_retry(
    adapter: &BybitAdapter,
    submission: OrderSubmission,
    timeout: Duration,
) -> Result<gateway::VenueOrderAck> {
    let deadline = Instant::now() + timeout;
    loop {
        match adapter.submit_order(submission.clone()).await {
            Ok(ack) => return Ok(ack),
            Err(GatewayError::VenueNotConnected { .. }) if Instant::now() < deadline => {
                sleep(Duration::from_secs(1)).await;
            }
            Err(err) => return Err(anyhow!(err)),
        }
    }
}

async fn wait_for_order_presence(
    adapter: &BybitAdapter,
    venue_order_id: &gateway::VenueOrderId,
    should_exist: bool,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let exists = adapter
            .query_open_orders()
            .await?
            .into_iter()
            .any(|order| order.venue_order_id == *venue_order_id);
        if exists == should_exist {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let expectation = if should_exist {
                "appear in"
            } else {
                "disappear from"
            };
            bail!(
                "timed out waiting for {} to {} open orders",
                venue_order_id,
                expectation
            );
        }
        sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_fill(
    fills: &mut tokio::sync::mpsc::Receiver<gateway::VenueFill>,
    client_order_id: &ClientOrderId,
    timeout: Duration,
) -> Result<gateway::VenueFill> {
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        if now >= deadline {
            bail!("timed out waiting for fill for {}", client_order_id);
        }

        let remaining = deadline - now;
        let received = tokio::time::timeout(remaining, fills.recv())
            .await
            .context("timed out waiting on fill receiver")?;
        let Some(fill) = received else {
            bail!("fill channel closed while waiting for {}", client_order_id);
        };

        if fill.client_order_id == *client_order_id {
            return Ok(fill);
        }
    }
}
