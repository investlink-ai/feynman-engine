use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};

use anyhow::{Context, Result};
use chrono::Utc;
use proptest::prelude::*;
use risk::{AgentRiskManager, EvaluationPath};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use types::{
    AgentAllocation, AgentId, AgentRiskLimits, AgentStatus, ExecHint, FirmBook, InstrumentId,
    InstrumentRiskLimits, OrderCore, OrderId, OrderType, PipelineOrder, PredictionExposureSummary,
    RiskOutcome, Side, TimeInForce, Validated, VenueId, VenueRiskLimits,
};

const SATOSHI: &str = "satoshi";
const GRAHAM: &str = "graham";
const BTC_USD: &str = "BTC-USD";
const BYBIT: &str = "bybit";

#[tokio::test]
async fn test_signal_without_stop_loss_rejected() -> Result<()> {
    let manager = make_risk_manager(dec!(100_000));
    let book = make_firm_book(dec!(100_000), Decimal::ZERO);
    let order = make_order_with(
        AgentId(SATOSHI.into()),
        dec!(5),
        dec!(100),
        None,
        Some(dec!(130)),
    );

    let outcome = manager.evaluate(&order, &book, EvaluationPath::SubmitSignal);

    assert_rejected_with(&outcome, "signal_stop_loss_required")?;
    Ok(())
}

#[tokio::test]
async fn test_order_without_stop_loss_uses_full_notional() -> Result<()> {
    let manager = make_risk_manager(dec!(100_000));
    let book = make_firm_book(dec!(100_000), Decimal::ZERO);
    let order = make_order(dec!(11), None);

    let outcome = manager.evaluate(&order, &book, EvaluationPath::SubmitOrder);

    assert_resized_with(&outcome, dec!(10), "account_risk_pct")?;
    Ok(())
}

#[tokio::test]
async fn test_signal_risk_reward_below_threshold_rejected() -> Result<()> {
    let manager = make_risk_manager(dec!(100_000));
    let book = make_firm_book(dec!(100_000), Decimal::ZERO);
    let order = make_order_with(
        AgentId(SATOSHI.into()),
        dec!(5),
        dec!(100),
        Some(dec!(90)),
        Some(dec!(115)),
    );

    let outcome = manager.evaluate(&order, &book, EvaluationPath::SubmitSignal);

    assert_rejected_with(&outcome, "signal_risk_reward_ratio")?;
    Ok(())
}

#[tokio::test]
async fn test_position_sizing_cap_triggers_resize() -> Result<()> {
    let manager = make_risk_manager(dec!(100_000));
    let book = make_firm_book(dec!(100_000), Decimal::ZERO);
    let order = make_order_with(
        AgentId(SATOSHI.into()),
        dec!(60),
        dec!(100),
        Some(dec!(99)),
        None,
    );

    let outcome = manager.evaluate(&order, &book, EvaluationPath::SubmitOrder);

    assert_resized_with(&outcome, dec!(50), "position_notional_pct")?;
    Ok(())
}

#[tokio::test]
async fn test_drawdown_halt_rejects_all() -> Result<()> {
    let manager = make_risk_manager(dec!(100_000));
    let book = make_firm_book(dec!(100_000), dec!(-0.16));
    let order = make_order(dec!(5), Some(dec!(95)));

    let outcome = manager.evaluate(&order, &book, EvaluationPath::SubmitOrder);

    let violations = rejected_violations(&outcome)?;
    let drawdown_violation = violations
        .iter()
        .find(|violation| violation.check_name == "firm_drawdown_pct")
        .context("missing drawdown halt violation")?;
    anyhow::ensure!(
        drawdown_violation.suggested_action.starts_with("halt_all:"),
        "expected halt_all action, got {}",
        drawdown_violation.suggested_action
    );
    Ok(())
}

#[tokio::test]
async fn test_per_agent_budget_isolation() -> Result<()> {
    let manager = make_risk_manager_with_caps(dec!(100_000), dec!(400), dec!(10_000));
    let book = make_firm_book(dec!(100_000), Decimal::ZERO);
    let satoshi_order = make_order(dec!(5), Some(dec!(95)));
    let graham_order = make_order_with(
        AgentId(GRAHAM.into()),
        dec!(5),
        dec!(100),
        Some(dec!(95)),
        None,
    );

    let satoshi_outcome = manager.evaluate(&satoshi_order, &book, EvaluationPath::SubmitOrder);
    let graham_outcome = manager.evaluate(&graham_order, &book, EvaluationPath::SubmitOrder);

    assert_rejected_with(&satoshi_outcome, "agent_max_gross_notional")?;
    assert_approved(&graham_outcome)?;
    Ok(())
}

#[tokio::test]
async fn test_valid_order_approved() -> Result<()> {
    let manager = make_risk_manager(dec!(100_000));
    let book = make_firm_book(dec!(100_000), Decimal::ZERO);
    let order = make_order_with(
        AgentId(SATOSHI.into()),
        dec!(5),
        dec!(100),
        Some(dec!(95)),
        Some(dec!(110)),
    );

    let outcome = manager.evaluate(&order, &book, EvaluationPath::SubmitSignal);

    assert_approved(&outcome)?;
    Ok(())
}

proptest! {
    #[test]
    #[ignore]
    fn prop_valid_inputs_never_panic(
        nav in 50_000_u64..500_000_u64,
        qty in 1_u64..200_u64,
        price in 50_u64..2_000_u64,
        stop_gap in 1_u64..20_u64,
        reward_gap in 2_u64..100_u64,
    ) {
        let nav = Decimal::from(nav);
        let qty = Decimal::from(qty);
        let price = Decimal::from(price);
        let stop_gap = Decimal::from(stop_gap);
        let reward_gap = Decimal::from(reward_gap);
        let manager = make_risk_manager(nav);
        let book = make_firm_book(nav, Decimal::ZERO);
        let stop_loss = price - stop_gap;
        let take_profit = price + reward_gap;
        prop_assume!(stop_loss > Decimal::ZERO);

        let order = make_order_with(
            AgentId(SATOSHI.into()),
            qty,
            price,
            Some(stop_loss),
            Some(take_profit),
        );

        let evaluation = panic::catch_unwind(AssertUnwindSafe(|| {
            manager.evaluate(&order, &book, EvaluationPath::SubmitSignal)
        }));

        prop_assert!(evaluation.is_ok(), "evaluate() panicked on valid input");
    }

    #[test]
    #[ignore]
    fn prop_risk_outcomes_remain_well_formed(
        nav in 50_000_u64..500_000_u64,
        qty in 1_u64..200_u64,
        price in 50_u64..2_000_u64,
        stop_gap in 1_u64..20_u64,
        reward_gap in 2_u64..100_u64,
    ) {
        let nav = Decimal::from(nav);
        let qty = Decimal::from(qty);
        let price = Decimal::from(price);
        let stop_gap = Decimal::from(stop_gap);
        let reward_gap = Decimal::from(reward_gap);
        let manager = make_risk_manager(nav);
        let book = make_firm_book(nav, Decimal::ZERO);
        let stop_loss = price - stop_gap;
        let take_profit = price + reward_gap;
        prop_assume!(stop_loss > Decimal::ZERO);

        let order = make_order_with(
            AgentId(SATOSHI.into()),
            qty,
            price,
            Some(stop_loss),
            Some(take_profit),
        );

        match manager.evaluate(&order, &book, EvaluationPath::SubmitSignal) {
            RiskOutcome::Approved { warnings } => {
                prop_assert!(warnings.is_empty(), "approved results should not emit warnings");
            }
            RiskOutcome::Resized {
                new_qty,
                warnings,
                ..
            } => {
                prop_assert!(new_qty > Decimal::ZERO, "resize qty must stay positive");
                prop_assert!(new_qty < qty, "resize qty must shrink the original order");
                prop_assert!(!warnings.is_empty(), "resized results must explain the resize");
                prop_assert!(warnings.iter().all(|warning| matches!(warning.violation_type, types::ViolationType::Soft)));
            }
            RiskOutcome::Rejected { violations } => {
                prop_assert!(!violations.is_empty(), "rejections must include at least one violation");
                prop_assert!(violations.iter().all(|violation| matches!(violation.violation_type, types::ViolationType::Hard)));
            }
        }
    }
}

fn make_firm_book(nav: Decimal, drawdown_pct: Decimal) -> FirmBook {
    let satoshi = AgentId(SATOSHI.into());
    let graham = AgentId(GRAHAM.into());

    FirmBook {
        nav,
        gross_notional: Decimal::ZERO,
        net_notional: Decimal::ZERO,
        realized_pnl: Decimal::ZERO,
        unrealized_pnl: Decimal::ZERO,
        daily_pnl: Decimal::ZERO,
        hourly_pnl: Decimal::ZERO,
        current_drawdown_pct: drawdown_pct,
        allocated_capital: dec!(20_000),
        cash_available: nav / dec!(2),
        total_fees_paid: Decimal::ZERO,
        agent_allocations: HashMap::from([
            (satoshi, make_agent_allocation(dec!(10_000))),
            (graham, make_agent_allocation(dec!(10_000))),
        ]),
        instruments: Vec::new(),
        prediction_exposure: PredictionExposureSummary {
            total_notional: Decimal::ZERO,
            pct_of_nav: Decimal::ZERO,
            unresolved_markets: 0,
        },
        as_of: Utc::now(),
    }
}

fn make_order(qty: Decimal, stop_loss: Option<Decimal>) -> PipelineOrder<Validated> {
    make_order_with(AgentId(SATOSHI.into()), qty, dec!(100), stop_loss, None)
}

fn make_order_with(
    agent_id: AgentId,
    qty: Decimal,
    price: Decimal,
    stop_loss: Option<Decimal>,
    take_profit: Option<Decimal>,
) -> PipelineOrder<Validated> {
    PipelineOrder::new(OrderCore {
        id: OrderId(format!("ord-{agent_id}-{qty}")),
        basket_id: None,
        agent_id,
        instrument_id: InstrumentId(BTC_USD.into()),
        venue_id: VenueId(BYBIT.into()),
        side: Side::Buy,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::GTC,
        qty,
        price: Some(price),
        trigger_price: None,
        stop_loss,
        take_profit,
        post_only: false,
        reduce_only: false,
        dry_run: true,
        exec_hint: ExecHint::default(),
        created_at: Utc::now(),
    })
    .into_validated(Utc::now())
}

fn make_risk_manager(nav: Decimal) -> AgentRiskManager {
    make_risk_manager_with_caps(nav, dec!(10_000), dec!(10_000))
}

fn make_risk_manager_with_caps(
    nav: Decimal,
    satoshi_max_gross_notional: Decimal,
    graham_max_gross_notional: Decimal,
) -> AgentRiskManager {
    let satoshi = AgentId(SATOSHI.into());
    let graham = AgentId(GRAHAM.into());
    let instrument = InstrumentId(BTC_USD.into());
    let venue = VenueId(BYBIT.into());

    AgentRiskManager::new(
        nav,
        HashMap::from([
            (
                satoshi,
                AgentRiskLimits {
                    allocated_capital: dec!(10_000),
                    max_position_notional: dec!(10_000),
                    max_gross_notional: satoshi_max_gross_notional,
                    max_drawdown_pct: dec!(0.15),
                    max_daily_loss: dec!(2_000),
                    max_open_orders: 10,
                    allowed_instruments: Some(vec![instrument.clone()]),
                    allowed_venues: Some(vec![venue.clone()]),
                },
            ),
            (
                graham,
                AgentRiskLimits {
                    allocated_capital: dec!(10_000),
                    max_position_notional: dec!(10_000),
                    max_gross_notional: graham_max_gross_notional,
                    max_drawdown_pct: dec!(0.15),
                    max_daily_loss: dec!(2_000),
                    max_open_orders: 10,
                    allowed_instruments: Some(vec![instrument.clone()]),
                    allowed_venues: Some(vec![venue.clone()]),
                },
            ),
        ]),
        HashMap::from([(
            instrument,
            InstrumentRiskLimits {
                instrument: InstrumentId(BTC_USD.into()),
                max_net_qty: dec!(1_000),
                max_gross_qty: dec!(1_000),
                max_concentration_pct: dec!(0.50),
                max_leverage: dec!(10),
            },
        )]),
        HashMap::from([(
            venue,
            VenueRiskLimits {
                venue: VenueId(BYBIT.into()),
                max_notional: nav,
                max_positions: 10,
                max_pct_of_nav: dec!(0.50),
            },
        )]),
    )
}

fn make_agent_allocation(capital: Decimal) -> AgentAllocation {
    AgentAllocation {
        allocated_capital: capital,
        used_capital: Decimal::ZERO,
        free_capital: capital,
        realized_pnl: Decimal::ZERO,
        unrealized_pnl: Decimal::ZERO,
        current_drawdown: Decimal::ZERO,
        max_drawdown_limit: dec!(-0.15),
        status: AgentStatus::Active,
    }
}

fn assert_approved(outcome: &RiskOutcome) -> Result<()> {
    match outcome {
        RiskOutcome::Approved { warnings } => {
            anyhow::ensure!(
                warnings.is_empty(),
                "expected no warnings, got {warnings:?}"
            );
            Ok(())
        }
        other => anyhow::bail!("expected approved outcome, got {other:?}"),
    }
}

fn assert_resized_with(
    outcome: &RiskOutcome,
    expected_qty: Decimal,
    expected_warning: &str,
) -> Result<()> {
    match outcome {
        RiskOutcome::Resized {
            new_qty, warnings, ..
        } => {
            anyhow::ensure!(
                *new_qty == expected_qty,
                "expected resize qty {expected_qty}, got {new_qty}"
            );
            anyhow::ensure!(
                warnings
                    .iter()
                    .any(|warning| warning.check_name == expected_warning),
                "missing resize warning {expected_warning}: {warnings:?}"
            );
            Ok(())
        }
        other => anyhow::bail!("expected resized outcome, got {other:?}"),
    }
}

fn assert_rejected_with(outcome: &RiskOutcome, expected_violation: &str) -> Result<()> {
    let violations = rejected_violations(outcome)?;
    anyhow::ensure!(
        violations
            .iter()
            .any(|violation| violation.check_name == expected_violation),
        "missing rejection violation {expected_violation}: {violations:?}"
    );
    Ok(())
}

fn rejected_violations(outcome: &RiskOutcome) -> Result<&[types::RiskViolation]> {
    match outcome {
        RiskOutcome::Rejected { violations } => Ok(violations.as_slice()),
        other => anyhow::bail!("expected rejected outcome, got {other:?}"),
    }
}
