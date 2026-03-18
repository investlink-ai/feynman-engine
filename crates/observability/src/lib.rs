//! observability — Metrics (Prometheus), tracing (OpenTelemetry), and dashboard traits.
//!
//! Provides observability into engine operations:
//! - MetricsCollector: Prometheus metrics (counters, gauges, histograms)
//! - DashboardPublisher: Real-time events for web UI
//! - Tracing: OpenTelemetry spans for distributed tracing

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

pub use types::{AgentId, VenueId, OrderId};

/// ─── Metrics Collector ───
///
/// Records Prometheus metrics for monitoring and alerting.
/// Thread-safe and non-blocking.
pub trait MetricsCollector: Send + Sync {
    /// Record order submission
    fn record_order_submitted(&self, venue: &str, agent: &str);

    /// Record order fill with slippage
    fn record_order_filled(&self, venue: &str, agent: &str, slippage_bps: f64);

    /// Record order rejection
    fn record_order_rejected(&self, venue: &str, agent: &str, reason: &str);

    /// Record latency from submission to fill
    fn record_fill_latency(&self, venue: &str, latency_ms: f64);

    /// Record time spent in risk gate evaluation
    fn record_risk_check_latency(&self, latency_us: f64);

    /// Set gauge: current position quantity
    fn set_position(&self, venue: &str, instrument: &str, agent: &str, qty: f64);

    /// Set gauge: agent P&L
    fn set_pnl(&self, agent: &str, pnl_type: &str, value: f64);

    /// Set gauge: firm net asset value
    fn set_nav(&self, value: f64);

    /// Record reconciliation divergence detected
    fn record_reconciliation_divergence(&self, venue: &str, instrument: &str);

    /// Set gauge: venue connection status (0 = disconnected, 1 = connected)
    fn set_venue_connection(&self, venue: &str, connected: bool);

    /// Record message bus activity
    fn record_bus_message(&self, topic: &str, direction: &str);

    /// Set gauge: pending messages in bus topic/group
    fn set_bus_pending(&self, topic: &str, group: &str, count: u64);

    /// Record circuit breaker trip
    fn record_circuit_breaker_trip(&self, breaker: &str);
}

/// ─── Dashboard Publisher ───
///
/// Publishes real-time events to web UI via SSE or WebSocket.
#[async_trait::async_trait]
pub trait DashboardPublisher: Send + Sync {
    /// Publish an update to the dashboard.
    /// Called whenever state changes (fills, positions, risk limits, etc).
    async fn publish(&self, update: DashboardUpdate) -> std::result::Result<(), anyhow::Error>;
}

/// Dashboard update event.
#[derive(Debug, Clone)]
pub enum DashboardUpdate {
    /// Firm-level state changed
    FirmBookChanged(FirmBook),
    /// Position quantity changed
    PositionChanged(InstrumentExposure),
    /// Order event (submitted, filled, cancelled, etc)
    OrderEvent(DashboardEvent),
    /// Venue connection status changed
    VenueStatusChanged(VenueStatus),
    /// Risk limit violated or breached
    RiskAlert(RiskViolation),
    /// Agent status changed (Active, Paused, etc)
    AgentStatusChanged { agent: AgentId, status: AgentStatusUpdate },
}

/// Firm-level positions and P&L.
#[derive(Debug, Clone)]
pub struct FirmBook {
    pub nav: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub gross_notional: Decimal,
    pub net_notional: Decimal,
    pub daily_pnl: Decimal,
    pub current_drawdown_pct: Decimal,
}

/// Aggregate exposure for a single instrument across all venues and agents.
#[derive(Debug, Clone)]
pub struct InstrumentExposure {
    pub instrument: String,
    pub net_qty: Decimal,
    pub notional: Decimal,
    pub venues: Vec<String>,
    pub agents: Vec<String>,
}

/// Dashboard audit event.
#[derive(Debug, Clone)]
pub struct DashboardEvent {
    pub event_type: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
    pub order_id: Option<OrderId>,
    pub agent: Option<AgentId>,
}

/// Venue connection and latency status.
#[derive(Debug, Clone)]
pub struct VenueStatus {
    pub venue: VenueId,
    pub connected: bool,
    pub latency_ms: Option<f64>,
    pub open_orders: u32,
    pub balance_usd: Option<Decimal>,
    pub last_heartbeat: Option<DateTime<Utc>>,
}

/// Risk limit approaching or breached.
#[derive(Debug, Clone)]
pub struct RiskViolation {
    pub limit_name: String,
    pub severity: RiskSeverity,
    pub message: String,
}

/// Risk severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskSeverity {
    /// Approaching limit (>80% utilization)
    Warning,
    /// Limit breached (>100% utilization)
    Critical,
    /// Loss-based halt (circuit breaker)
    Emergency,
}

/// Agent status update.
#[derive(Debug, Clone)]
pub struct AgentStatusUpdate {
    pub is_active: bool,
    pub reason: Option<String>,
}

/// ─── Tracing (OpenTelemetry) ───
///
/// Every order gets a trace span from signal to fill.
/// Uses order_id as trace_id for correlation.
///
/// Typical span hierarchy:
/// ```text
/// Signal Received [span]
///   └── Risk Check [span]
///   └── Strategy Selected [span]
///   └── Venue Submitted [span]
///       └── Fill Received [span]
/// ```
///
/// All spans share the same trace_id (= order_id) for correlation.

/// Span context for OpenTelemetry tracing.
#[derive(Debug, Clone)]
pub struct SpanContext {
    /// Trace ID (= OrderId for order spans)
    pub trace_id: String,
    /// Span ID (unique within trace)
    pub span_id: String,
    /// Parent span ID (if any)
    pub parent_span_id: Option<String>,
}

/// Helper trait for span recording (minimal interface).
pub trait TraceRecorder: Send + Sync {
    /// Record a span event.
    fn record_span(&self, name: &str, context: &SpanContext, duration_us: u64);

    /// Record an attribute on the current span.
    fn record_attribute(&self, key: &str, value: &str);

    /// Record an event on the current span.
    fn record_event(&self, event: &str);
}
