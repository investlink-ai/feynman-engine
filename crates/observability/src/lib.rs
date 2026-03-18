//! observability — Metrics (Prometheus), tracing (OpenTelemetry), dashboard, and health reporting.
//!
//! All financial values in metrics use `Decimal` — no `f64` for money.
//! Prometheus metrics use f64 (Prometheus wire format requires it),
//! but the conversion happens at the boundary, not in business logic.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

pub use types::{
    AgentId, EngineHealth, OrderId, OverallHealth, SubsystemHealth, SubsystemId, VenueId,
};

// ─── Metrics Collector ───

/// Records Prometheus metrics for monitoring and alerting.
///
/// Financial values are passed as `Decimal` and converted to f64
/// at the Prometheus boundary only. No f64 in business logic.
pub trait MetricsCollector: Send + Sync {
    /// Record order submission.
    fn record_order_submitted(&self, venue: &VenueId, agent: &AgentId);

    /// Record order fill.
    fn record_order_filled(&self, venue: &VenueId, agent: &AgentId, slippage_bps: Decimal);

    /// Record order rejection.
    fn record_order_rejected(&self, venue: &VenueId, agent: &AgentId, reason: &str);

    /// Record latency from submission to fill (milliseconds).
    fn record_fill_latency(&self, venue: &VenueId, latency_ms: u64);

    /// Record time spent in risk gate evaluation (microseconds).
    fn record_risk_check_latency(&self, latency_us: u64);

    /// Set gauge: current position quantity.
    fn set_position(&self, venue: &VenueId, instrument: &str, agent: &AgentId, qty: Decimal);

    /// Set gauge: agent P&L.
    fn set_pnl(&self, agent: &AgentId, pnl_type: PnlType, value: Decimal);

    /// Set gauge: firm net asset value.
    fn set_nav(&self, value: Decimal);

    /// Record reconciliation divergence detected.
    fn record_reconciliation_divergence(&self, venue: &VenueId, instrument: &str);

    /// Set venue connection status.
    fn set_venue_connection(&self, venue: &VenueId, state: &types::ConnectionState);

    /// Record message bus activity.
    fn record_bus_message(&self, topic: &str, direction: BusDirection);

    /// Set gauge: pending messages in bus topic/group.
    fn set_bus_pending(&self, topic: &str, group: &str, count: u64);

    /// Record circuit breaker trip.
    fn record_circuit_breaker_trip(&self, breaker: &str);

    /// Record subsystem health change.
    fn record_subsystem_health(&self, subsystem: SubsystemId, health: &SubsystemHealth);

    /// Record mark-to-market update.
    fn record_mtm_update(&self, instrument: &str, unrealized_pnl: Decimal);
}

/// P&L metric type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PnlType {
    Realized,
    Unrealized,
    Daily,
}

/// Bus message direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusDirection {
    Published,
    Received,
}

// ─── Dashboard Publisher ───

/// Publishes real-time events to web UI via SSE or WebSocket.
#[async_trait::async_trait]
pub trait DashboardPublisher: Send + Sync {
    async fn publish(&self, update: DashboardUpdate) -> std::result::Result<(), anyhow::Error>;
}

/// Dashboard update event.
#[derive(Debug, Clone)]
pub enum DashboardUpdate {
    FirmBookChanged(FirmBookSummary),
    PositionChanged(InstrumentExposureSummary),
    OrderEvent(DashboardEvent),
    VenueStatusChanged(VenueStatus),
    RiskAlert(RiskAlert),
    AgentStatusChanged {
        agent: AgentId,
        status: AgentStatusUpdate,
    },
    HealthChanged(EngineHealth),
}

/// Firm-level summary for dashboard.
#[derive(Debug, Clone)]
pub struct FirmBookSummary {
    pub nav: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub gross_notional: Decimal,
    pub net_notional: Decimal,
    pub daily_pnl: Decimal,
    pub current_drawdown_pct: Decimal,
}

/// Instrument exposure summary for dashboard.
#[derive(Debug, Clone)]
pub struct InstrumentExposureSummary {
    pub instrument: String,
    pub net_qty: Decimal,
    pub notional: Decimal,
    pub unrealized_pnl: Decimal,
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

/// Venue connection and latency status for dashboard.
#[derive(Debug, Clone)]
pub struct VenueStatus {
    pub venue: VenueId,
    pub state: types::ConnectionState,
    pub heartbeat_latency_ms: Option<u32>,
    pub open_orders: u32,
    pub balance_usd: Option<Decimal>,
    pub last_heartbeat: Option<DateTime<Utc>>,
}

/// Risk alert for dashboard.
#[derive(Debug, Clone)]
pub struct RiskAlert {
    pub limit_name: String,
    pub severity: RiskSeverity,
    pub message: String,
    pub agent: Option<AgentId>,
}

/// Risk severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskSeverity {
    Warning,
    Critical,
    Emergency,
}

/// Agent status update for dashboard.
#[derive(Debug, Clone)]
pub struct AgentStatusUpdate {
    pub is_active: bool,
    pub reason: Option<String>,
}

// ─── Tracing (OpenTelemetry) ───

/// Span context for OpenTelemetry tracing.
#[derive(Debug, Clone)]
pub struct SpanContext {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
}

/// Helper trait for span recording.
pub trait TraceRecorder: Send + Sync {
    fn record_span(&self, name: &str, context: &SpanContext, duration_us: u64);
    fn record_attribute(&self, key: &str, value: &str);
    fn record_event(&self, event: &str);
}
