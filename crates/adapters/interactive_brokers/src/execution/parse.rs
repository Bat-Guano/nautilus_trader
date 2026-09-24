// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Parsing utilities for converting IB execution data to Nautilus reports.

use std::str::FromStr;

use ahash::AHashMap;
use anyhow::Context;
use ibapi::orders::{Execution, OrderData, OrderStatus};
use jiff::{
    Timestamp,
    civil::DateTime,
    tz::{AmbiguousOffset, Offset},
};
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_model::{
    enums::{
        LiquiditySide, OrderSide, OrderStatus as NautilusOrderStatus, OrderType, TimeInForce,
        TrailingOffsetType,
    },
    identifiers::{AccountId, ClientOrderId, InstrumentId, TradeId, VenueOrderId},
    instruments::Instrument,
    reports::{FillReport, OrderStatusReport},
    types::{Currency, Money, Price, Quantity},
};
use rust_decimal::Decimal;

use crate::{
    common::{
        enums::{IbAction, IbOrderStatus, IbOrderType, IbTimeInForce},
        parse::is_spread_instrument_id,
    },
    execution::account::raw_ib_account_code,
    providers::instruments::InteractiveBrokersInstrumentProvider,
};

pub(crate) fn should_use_avg_fill_price(avg_fill_price: f64, instrument_id: &InstrumentId) -> bool {
    avg_fill_price.is_finite()
        && avg_fill_price != f64::MAX
        && avg_fill_price != 0.0
        && (avg_fill_price > 0.0 || is_spread_instrument_id(instrument_id))
}

pub(crate) fn ib_venue_order_id(order_id: i32, perm_id: i64) -> VenueOrderId {
    if perm_id != 0 {
        VenueOrderId::new(format!("PERM-{perm_id}"))
    } else {
        VenueOrderId::new(order_id.to_string())
    }
}

pub(crate) fn normalized_order_ref(order_ref: &str) -> Option<&str> {
    if order_ref.is_empty() {
        return None;
    }

    Some(
        order_ref
            .rsplit_once(':')
            .map_or(order_ref, |(base, _)| base),
    )
}

/// Parse an IB execution to a Nautilus FillReport.
///
/// # Errors
///
/// Returns an error if parsing fails.
///
/// # Note
///
/// The `avg_px` parameter is stored from order status updates and is available for
/// future use when FillReport supports additional metadata fields.
#[allow(clippy::too_many_arguments)]
pub fn parse_execution_to_fill_report(
    execution: &Execution,
    _contract: &ibapi::contracts::Contract,
    commission: f64,
    commission_currency: &str,
    instrument_id: InstrumentId,
    account_id: AccountId,
    instrument_provider: &InteractiveBrokersInstrumentProvider,
    ts_init: UnixNanos,
    avg_px: Option<Price>,
) -> anyhow::Result<FillReport> {
    // Get price magnifier from instrument provider
    let price_magnifier = instrument_provider.get_price_magnifier(&instrument_id) as f64;

    // Convert execution price
    let execution_price = execution.price * price_magnifier;

    // Determine order side
    let order_side = IbAction::from_str(execution.side.as_str())?.order_side();

    // Get instrument for precision
    let instrument = instrument_provider
        .find(&instrument_id)
        .context("Instrument not found")?;

    // Create quantities and prices
    let last_qty = Quantity::new(execution.shares, instrument.size_precision());
    let last_px = Price::new(execution_price, instrument.price_precision());

    // Clamp only IB's -1 pending sentinel to 0.0 to preserve rebates
    let commission_clamped = if commission == -1.0 { 0.0 } else { commission };
    let commission_money = Money::new(commission_clamped, Currency::from_str(commission_currency)?);

    // Parse execution time
    let ts_event = parse_execution_time(&execution.time)?;

    // Create trade ID
    let trade_id = TradeId::new(&execution.execution_id);

    let venue_order_id = ib_venue_order_id(execution.order_id, execution.perm_id);

    let client_order_id = normalized_order_ref(&execution.order_reference).map(ClientOrderId::new);

    let mut report = FillReport::new(
        account_id,
        instrument_id,
        venue_order_id,
        trade_id,
        order_side,
        last_qty,
        last_px,
        commission_money,
        LiquiditySide::NoLiquiditySide,
        client_order_id,
        None, // venue_position_id
        ts_event,
        ts_init,
        Some(nautilus_core::UUID4::new()),
    );
    report.avg_px = avg_px.map(|price: Price| price.as_decimal());

    Ok(report)
}

/// Parse an IB order status to a Nautilus OrderStatusReport.
///
/// # Errors
///
/// Returns an error if parsing fails.
pub fn parse_order_status_to_report(
    order_status: &OrderStatus,
    order: Option<&ibapi::orders::Order>,
    instrument_id: InstrumentId,
    account_id: AccountId,
    instrument_provider: &InteractiveBrokersInstrumentProvider,
    ts_init: UnixNanos,
) -> anyhow::Result<OrderStatusReport> {
    // C2.6: capture the exact raw IBKR status string before any
    // normalization. The patched ibapi `OrderStatusKind` preserves the
    // wire value byte-for-byte for known and unknown vocabulary alike.
    let raw_order_status = order_status.status.as_str().to_string();

    // Get price magnifier from instrument provider
    let price_magnifier = instrument_provider.get_price_magnifier(&instrument_id) as f64;

    let mut nautilus_status = match IbOrderStatus::from_str(order_status.status.as_str()) {
        Ok(status) => status.nautilus_status(),
        _ => {
            tracing::warn!(
                "Unknown order status: {}, defaulting to SUBMITTED",
                order_status.status.as_str()
            );
            NautilusOrderStatus::Submitted
        }
    };

    // Get order side
    let order_side = if let Some(order) = order {
        IbAction::from(order.action).order_side()
    } else {
        // Default to Buy if order not available
        OrderSide::Buy
    };

    let instrument = instrument_provider.find(&instrument_id);

    // Get instrument for precision (use 0 as default if not available)
    let size_precision = instrument
        .as_ref()
        .map_or(0, |instr| instr.size_precision());
    let price_precision = instrument
        .as_ref()
        .map_or(0, |instr| instr.price_precision());

    // Get quantity
    let quantity = if let Some(order) = order {
        Quantity::new(order.total_quantity, size_precision)
    } else {
        Quantity::zero(size_precision)
    };

    // Get filled quantity
    let filled_qty = Quantity::new(order_status.filled, size_precision);

    // Get average price
    let average_fill_price = order_status.average_fill_price.unwrap_or(0.0);
    let include_avg_px = should_use_avg_fill_price(average_fill_price, &instrument_id);
    let avg_px_value = if include_avg_px {
        average_fill_price * price_magnifier
    } else {
        0.0
    };

    if order_status.filled > 0.0
        && (order_status.remaining > 0.0
            || order.is_some_and(|order| order.total_quantity > order_status.filled))
    {
        nautilus_status = NautilusOrderStatus::PartiallyFilled;
    }

    let venue_order_id = ib_venue_order_id(order_status.order_id, order_status.perm_id);

    let client_order_id = order
        .and_then(|order| normalized_order_ref(&order.order_ref))
        .map(ClientOrderId::new);

    // Map order type from IB order if available
    let order_type = order
        .map(|order| map_ib_order_type(&order.order_type, order.limit_price))
        .unwrap_or(OrderType::Market);

    // Map time in force from IB order if available
    let time_in_force = if let Some(order) = order {
        let ib_time_in_force = IbTimeInForce::from(order.tif.clone());
        if ib_time_in_force == IbTimeInForce::GoodTilDate || !order.good_till_date.is_empty() {
            TimeInForce::Gtd
        } else {
            ib_time_in_force.nautilus_time_in_force()
        }
    } else {
        TimeInForce::Day // Default when order not available
    };

    // Parse limit price if available
    let mut report = OrderStatusReport::new(
        account_id,
        instrument_id,
        client_order_id,
        venue_order_id,
        order_side.into(),
        order_type,
        time_in_force,
        nautilus_status,
        quantity,
        filled_qty,
        ts_init, // ts_accepted
        ts_init, // ts_last
        ts_init,
        Some(nautilus_core::UUID4::new()), // report_id
    );

    // Preserve the exact raw broker provenance alongside the normalized
    // status; C2.4 adjudicates whether that provenance is trusted.
    report = report.with_raw_order_status(raw_order_status);

    // Set optional fields
    if let Some(order) = order {
        if let Some(limit_price) = order.limit_price {
            let converted = limit_price * price_magnifier;
            report = report.with_price(Price::new(converted, price_precision));
        }

        let (trigger_price, limit_offset, trailing_offset, trailing_offset_type) =
            parse_ib_order_pricing_fields(order, order_type, price_magnifier, price_precision)?;

        if let Some(trigger_price) = trigger_price {
            report = report.with_trigger_price(trigger_price);
        }

        if let Some(limit_offset) = limit_offset {
            report = report.with_limit_offset(limit_offset);
        }

        if let Some(trailing_offset) = trailing_offset {
            report = report.with_trailing_offset(trailing_offset);
        }

        if let Some(trailing_offset_type) = trailing_offset_type {
            report = report.with_trailing_offset_type(trailing_offset_type);
        }
    }

    if include_avg_px {
        report = report.with_avg_px(decimal_from_f64(avg_px_value)?);
    }

    Ok(report)
}

/// Returns the broker `perm_id` when it is a usable venue identity.
///
/// A broker `perm_id` is authoritative only when it is **strictly positive**.
/// IBKR reports `perm_id == 0` for an order the broker has not yet acknowledged,
/// in which case the only venue identifier available is the API order id and no
/// broker venue identity exists. A non-positive value (including a negative
/// sentinel such as `-1`) is not valid broker venue identity either: it must
/// never become a `PERM-{perm_id}` venue id or be treated as strong identity by
/// the reconciliation merge. Callers must carry this distinction explicitly
/// rather than parse venue-id strings later.
#[must_use]
pub fn broker_perm_id(perm_id: i64) -> Option<i64> {
    (perm_id > 0).then_some(perm_id)
}

/// A single IBKR order observation bound for one reconciliation snapshot.
///
/// The open-order and completed-order sources do not always expose the same
/// identity strength: an open-order record can arrive without a usable broker
/// `perm_id`, so its Nautilus `venue_order_id` is only the API order-id
/// fallback. The tag travels with the report so distinct orders are never
/// collapsed on a venue id that does not represent broker venue identity.
#[derive(Clone, Debug)]
pub struct OrderReportObservation {
    /// The report to publish on the existing reconciliation seam.
    pub report: OrderStatusReport,
    /// Strictly positive broker `perm_id` the report's venue identity was
    /// derived from, or `None` when no authoritative broker venue identity was
    /// available (zero, negative, or absent) and only the API order-id fallback
    /// exists.
    pub broker_perm_id: Option<i64>,
}

/// Parse an IBKR completed-order record into a Nautilus [`OrderStatusReport`].
///
/// Completed-order records are the broker's explicit record of orders which
/// have left the open-order book. They are the only IBKR source that reports
/// terminal cancellation or completion, so they are the evidence used to turn a
/// completed or cancelled broker order into an `OrderStatusReport`.
///
/// Every identity in the produced report comes from broker evidence:
///
/// * `venue_order_id` is derived from a strictly positive broker `perm_id`
///   (`PERM-{perm_id}`). A zero or negative `perm_id` is not authoritative venue
///   identity and is rejected;
/// * `client_order_id` is the normalized client `order_ref`;
/// * `account_id` is the configured Nautilus account identity, accepted only
///   when the record's broker account matches it exactly.
///
/// # Errors
///
/// Returns an error when the record cannot support a safe reconciliation report.
/// The caller must then skip the record; it must not invent the missing data:
///
/// * the record carries no strictly positive broker `perm_id`, so no
///   authoritative venue identity exists and no venue id is synthesized;
/// * the record carries no broker account, so its account provenance is unknown;
/// * the record's broker account is not the configured account;
/// * the record carries no usable client `order_ref`, so the report could not
///   carry the `client_order_id` the reconciliation seam binds downstream state
///   through;
/// * the raw broker status is outside the mapped IBKR vocabulary. A completed
///   record whose status cannot be classified must not be published with a
///   defaulted non-terminal status, and must not be guessed into a terminal
///   one;
/// * the raw broker status is a known *non-terminal* status. The completed-order
///   source is terminal broker evidence, so a working state reported by it is
///   contradictory and is not promoted into terminal authority.
pub fn parse_completed_order_to_report(
    order_data: &OrderData,
    instrument_id: InstrumentId,
    account_id: AccountId,
    instrument_provider: &InteractiveBrokersInstrumentProvider,
    ts_init: UnixNanos,
) -> anyhow::Result<OrderStatusReport> {
    let order = &order_data.order;

    // Venue identity must originate from broker evidence. A completed record
    // carries no live API order id (`order_data.order_id` is the legacy `-1`
    // sentinel), so `perm_id` is the only broker identity available; refuse the
    // record rather than fabricate one. Only a strictly positive broker `perm_id`
    // is authoritative: zero means IBKR has not acknowledged the order, and a
    // non-positive value is invalid broker identity that must never become a
    // `PERM-{perm_id}` venue id or be reported as terminal broker evidence.
    anyhow::ensure!(
        broker_perm_id(order.perm_id).is_some(),
        "IBKR completed order perm_id {} is not a positive broker venue identity; refusing to fabricate a venue order id",
        order.perm_id,
    );

    // Account provenance must also be broker evidence: an absent or foreign
    // broker account must never be silently attributed to the locally
    // configured Nautilus account.
    anyhow::ensure!(
        !order.account.is_empty(),
        "IBKR completed order carries no broker account; refusing to attribute it to the configured Nautilus account",
    );
    anyhow::ensure!(
        order.account == raw_ib_account_code(&account_id),
        "IBKR completed order does not belong to the configured Nautilus account",
    );

    // The reconciliation seam binds downstream state through `client_order_id`,
    // so a completed record without usable client order identity must not
    // produce a report. No local OMS identity, API order id, or `perm_id` is
    // substituted for it.
    anyhow::ensure!(
        normalized_order_ref(&order.order_ref).is_some_and(|value| !value.is_empty()),
        "IBKR completed order carries no usable client order_ref; refusing to publish a terminal report without client order identity",
    );

    let raw_order_status = order_data.order_state.status.as_str();

    // `parse_order_status_to_report` defaults an unrecognized raw status to
    // SUBMITTED. That is tolerable for a working order whose vocabulary has
    // moved on, but for a completed record it would publish fabricated
    // non-terminal broker truth. Fail closed instead.
    let ib_status = IbOrderStatus::from_str(raw_order_status).with_context(|| {
        format!(
            "IBKR completed order perm_id {} has an unmappable raw status '{raw_order_status}'",
            order.perm_id,
        )
    })?;

    // Completed orders are terminal broker evidence only. A known non-terminal
    // status on a completed record is contradictory broker evidence and must not
    // be promoted into terminal authority for this snapshot.
    anyhow::ensure!(
        ib_status.is_terminal(),
        "IBKR completed order perm_id {} reports the non-terminal status '{raw_order_status}'; the completed-order source accepts terminal evidence only",
        order.perm_id,
    );

    let mut report = parse_order_status_to_report(
        &OrderStatus {
            order_id: order_data.order_id,
            status: order_data.order_state.status.clone(),
            filled: order.filled_quantity,
            remaining: (order.total_quantity - order.filled_quantity).max(0.0),
            average_fill_price: None,
            perm_id: order.perm_id,
            parent_id: order.parent_id,
            last_fill_price: None,
            client_id: order.client_id,
            why_held: String::new(),
            market_cap_price: None,
        },
        Some(order),
        instrument_id,
        account_id,
        instrument_provider,
        ts_init,
    )?;

    // The record is terminal by construction, so the mapped broker status is
    // authoritative over the working-order partial-fill promotion: an order
    // cancelled after a partial fill is terminal at the broker, not still
    // working.
    report.order_status = ib_status.nautilus_status();

    anyhow::ensure!(
        report.client_order_id.is_some(),
        "IBKR completed order produced a report without client order identity",
    );

    Ok(report)
}

/// Whether an incoming observation supersedes one already merged for the same
/// logical broker order.
///
/// Deterministic precedence:
///
/// 1. stronger broker venue identity replaces the API order-id fallback;
/// 2. terminal broker evidence replaces a non-terminal observation;
/// 3. between two terminal observations that both carry broker venue identity,
///    the later (completed-order) observation wins - it is the explicit terminal
///    authority.
///
/// A terminal observation that carries only the API order-id fallback never
/// replaces a broker-identified observation, so a weaker identity can never
/// displace real broker venue identity.
fn supersedes(incoming: &OrderReportObservation, existing: &OrderReportObservation) -> bool {
    let stronger_venue_identity =
        incoming.broker_perm_id.is_some() && existing.broker_perm_id.is_none();
    let terminal_evidence =
        incoming.report.order_status.is_closed() && !existing.report.order_status.is_closed();
    let explicit_terminal_authority = incoming.report.order_status.is_closed()
        && existing.report.order_status.is_closed()
        && incoming.broker_perm_id.is_some()
        && existing.broker_perm_id.is_some();

    stronger_venue_identity || terminal_evidence || explicit_terminal_authority
}

/// Merge open-order and completed-order observations into one deterministic
/// reconciliation snapshot.
///
/// IBKR can report the same logical order from both `reqAllOpenOrders` and
/// `reqCompletedOrders` inside one reconciliation window: an order that
/// completes between the two requests is still returned by the open-order
/// snapshot. Two overlapping observations can disagree about venue identity,
/// because IBKR reports `perm_id == 0` until it acknowledges an order, so an
/// open-order observation may carry only the API order-id fallback while the
/// completed-order observation for the same order carries a real `perm_id`.
/// Only a strictly positive `perm_id` counts as broker venue identity, so a
/// zero or negative value is tagged as the fallback rather than as strong
/// identity.
///
/// Correlation uses only fields that both sources actually share and that are
/// unique to the logical order:
///
/// * primary key: the Nautilus venue identity ([`VenueOrderId`]) already derived
///   by the crate-private `ib_venue_order_id` helper from the broker `perm_id`;
/// * fallback: the `client_order_id` derived from the broker `order_ref`, used
///   **only** while exactly one of the two observations lacks broker venue
///   identity. Two observations that both carry a strictly positive broker
///   `perm_id` are never merged on client identity, so unrelated broker orders
///   cannot be collapsed.
///
/// Fields deliberately not used for correlation: the API client id and the
/// broker account (both are shared by every order of the session, so they cannot
/// identify an order), the live API order id (absent from completed records),
/// and instrument, price or quantity (not identity evidence at all).
///
/// Output ordering is deterministic: open-order observations keep their source
/// position (a replacement overwrites in place) and completed-only observations
/// are appended in broker arrival order. At most one report is emitted per
/// correlated logical order, so one snapshot cannot carry contradictory status
/// reports for the same broker order.
#[must_use]
pub fn merge_order_status_reports(
    open_observations: Vec<OrderReportObservation>,
    completed_observations: Vec<OrderReportObservation>,
) -> Vec<OrderStatusReport> {
    let mut merged: Vec<OrderReportObservation> =
        Vec::with_capacity(open_observations.len() + completed_observations.len());
    let mut index_by_venue: AHashMap<VenueOrderId, usize> = AHashMap::new();
    let mut index_by_client: AHashMap<ClientOrderId, usize> = AHashMap::new();

    for observation in open_observations.into_iter().chain(completed_observations) {
        let venue_order_id = observation.report.venue_order_id;
        let client_order_id = observation.report.client_order_id;

        let by_venue = index_by_venue.get(&venue_order_id).copied();
        let by_client = client_order_id.and_then(|id| index_by_client.get(&id).copied());

        let target = match (by_venue, by_client) {
            (Some(index), _) => Some(index),
            (None, Some(index)) => {
                // Venue-identity transition: correlate on the shared client
                // order identity only while exactly one observation lacks broker
                // venue identity.
                let existing = &merged[index];
                (existing.broker_perm_id.is_none() != observation.broker_perm_id.is_none())
                    .then_some(index)
            }
            (None, None) => None,
        };

        match target {
            Some(index) => {
                if !supersedes(&observation, &merged[index]) {
                    continue;
                }

                let previous = &merged[index];

                if previous.report.venue_order_id != venue_order_id {
                    index_by_venue.remove(&previous.report.venue_order_id);
                }

                if let Some(previous_client_id) = previous.report.client_order_id {
                    if Some(previous_client_id) != client_order_id
                        && index_by_client.get(&previous_client_id) == Some(&index)
                    {
                        index_by_client.remove(&previous_client_id);
                    }
                }

                index_by_venue.insert(venue_order_id, index);

                if let Some(client_order_id) = client_order_id {
                    index_by_client.entry(client_order_id).or_insert(index);
                }

                merged[index] = observation;
            }
            None => {
                index_by_venue.insert(venue_order_id, merged.len());

                if let Some(client_order_id) = client_order_id {
                    index_by_client
                        .entry(client_order_id)
                        .or_insert(merged.len());
                }

                merged.push(observation);
            }
        }
    }

    merged
        .into_iter()
        .map(|observation| observation.report)
        .collect()
}

fn map_ib_order_type(order_type: &str, limit_price: Option<f64>) -> OrderType {
    if order_type == "IBALGO" && limit_price.is_some_and(|price| price != 0.0) {
        OrderType::Limit
    } else {
        IbOrderType::from_str(order_type)
            .map_or(OrderType::Market, IbOrderType::nautilus_order_type)
    }
}

fn parse_ib_order_pricing_fields(
    order: &ibapi::orders::Order,
    order_type: OrderType,
    price_magnifier: f64,
    price_precision: u8,
) -> anyhow::Result<(
    Option<Price>,
    Option<Decimal>,
    Option<Decimal>,
    Option<TrailingOffsetType>,
)> {
    let mut trigger_price = None;
    let mut limit_offset = None;
    let mut trailing_offset = None;
    let mut trailing_offset_type = None;

    if matches!(
        order_type,
        OrderType::TrailingStopMarket | OrderType::TrailingStopLimit
    ) {
        if let Some(trail_stop_price) = order.trail_stop_price {
            trigger_price = Some(Price::new(
                trail_stop_price * price_magnifier,
                price_precision,
            ));
        }

        if let Some(aux_price) = order.aux_price {
            trailing_offset = Some(decimal_from_f64(aux_price)?);
            trailing_offset_type = Some(TrailingOffsetType::Price);
        } else if let Some(trailing_percent) = order.trailing_percent {
            trailing_offset = Some(decimal_from_f64(trailing_percent)? * Decimal::from(100));
            trailing_offset_type = Some(TrailingOffsetType::BasisPoints);
        }

        if order_type == OrderType::TrailingStopLimit
            && let Some(limit_price_offset) = order.limit_price_offset
        {
            limit_offset = Some(decimal_from_f64(limit_price_offset)?);
            trailing_offset_type = Some(trailing_offset_type.unwrap_or(TrailingOffsetType::Price));
        }

        return Ok((
            trigger_price,
            limit_offset,
            trailing_offset,
            trailing_offset_type,
        ));
    }

    if let Some(aux_price) = order.aux_price {
        trigger_price = Some(Price::new(aux_price * price_magnifier, price_precision));
    }

    Ok((
        trigger_price,
        limit_offset,
        trailing_offset,
        trailing_offset_type,
    ))
}

fn decimal_from_f64(value: f64) -> anyhow::Result<Decimal> {
    Decimal::from_str(&value.to_string())
        .with_context(|| format!("Failed to convert IB floating-point value {value} to Decimal"))
}

/// Parse execution time string to UnixNanos.
///
/// Parse IB execution time to UnixNanos.
///
/// Supported IB formats:
/// - "20230223 00:43:36 Universal"
/// - "20230223 00:43:36 UTC"
/// - "20230223 00:43:36 MET"
/// - "20230223 00:43:36 America/New_York"
/// - "20230223 00:43:36" (assumed UTC)
/// - "20250225-15:15:00" (assumed UTC)
///
/// Timezones are resolved through Jiff's bundled IANA tz database, so any
/// region abbreviation or name that IB stamps the execution with (e.g. `MET`,
/// `EST`, `America/New_York`) is honored, matching the v1 pandas-based parser.
/// This matters because some IB accounts (e.g. European paper accounts) report a
/// server timezone such as `MET` that the gateway cannot be coerced out of.
///
/// # Errors
///
/// Returns an error if the timestamp is malformed, the timezone is
/// unrecognized, or the local time is non-existent (a DST spring-forward gap).
/// DST fall-back folds resolve to the earliest matching instant.
pub fn parse_execution_time(time_str: &str) -> anyhow::Result<UnixNanos> {
    const NAIVE_FORMAT: &str = "%Y%m%d %H:%M:%S";

    // Hyphenated, space-less form (e.g. "20250225-15:15:00") is always UTC.
    if !time_str.contains(' ') {
        let normalized = time_str.replace('-', " ");
        let dt = DateTime::strptime(NAIVE_FORMAT, &normalized).map_err(|e| {
            anyhow::anyhow!("Failed to parse execution timestamp '{time_str}': {e}")
        })?;
        return datetime_to_unix_nanos(Offset::UTC.to_timestamp(dt)?, time_str);
    }

    // Split into at most three parts: date, time, and optional timezone token.
    // The timezone token itself never contains a space, so `splitn(3, ' ')`
    // correctly groups IANA names such as "America/New_York".
    let mut parts = time_str.splitn(3, ' ');
    let (Some(date), Some(time)) = (parts.next(), parts.next()) else {
        anyhow::bail!("Invalid execution time format: {time_str}");
    };
    let tz_str = parts.next().unwrap_or("").trim();

    let naive_str = format!("{date} {time}");
    let dt = DateTime::strptime(NAIVE_FORMAT, &naive_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse execution timestamp '{time_str}': {e}"))?;

    let utc = if tz_str.is_empty() {
        Offset::UTC.to_timestamp(dt)?
    } else {
        localize_with_zone(dt, tz_str, time_str)?
    };

    datetime_to_unix_nanos(utc, time_str)
}

/// Localize a naive timestamp against an IB timezone token and convert to UTC.
///
/// `Z` is normalized to `UTC`; everything else is resolved through the IANA tz
/// database. Error and fold behavior is documented on [`parse_execution_time`].
fn localize_with_zone(dt: DateTime, tz_str: &str, time_str: &str) -> anyhow::Result<Timestamp> {
    let tz_name = if tz_str.eq_ignore_ascii_case("Z") {
        "UTC"
    } else {
        tz_str
    };

    let zone = get_timezone(tz_name).map_err(|_| {
        anyhow::anyhow!(
            "Unrecognised execution timezone '{tz_str}' in '{time_str}'. Configure TWS / IB Gateway to emit a standard timezone (e.g. UTC)"
        )
    })?;
    let ambiguous = zone.to_ambiguous_timestamp(dt);
    match ambiguous.offset() {
        AmbiguousOffset::Unambiguous { .. } => Ok(ambiguous.unambiguous()?),
        // Fall-back fold: take the earliest instant (worst case ~1h skew).
        AmbiguousOffset::Fold { .. } => Ok(ambiguous.earlier()?),
        AmbiguousOffset::Gap { .. } => {
            anyhow::bail!("Execution timestamp '{time_str}' is non-existent in timezone '{tz_str}'")
        }
    }
}

fn datetime_to_unix_nanos(dt: Timestamp, time_str: &str) -> anyhow::Result<UnixNanos> {
    let nanos: u64 = dt
        .as_nanosecond()
        .try_into()
        .map_err(|_| anyhow::anyhow!("Execution timestamp '{time_str}' was before Unix epoch"))?;
    Ok(UnixNanos::new(nanos))
}

#[cfg(test)]
mod tests {
    use ibapi::{
        contracts::Contract,
        orders::{Action, ExecutionSide, Liquidity, Order, OrderStatusKind},
    };
    use nautilus_model::{
        enums::TrailingOffsetType,
        identifiers::{Symbol, Venue},
        instruments::{InstrumentAny, stubs::equity_aapl},
    };
    use rust_decimal::Decimal;

    use super::*;
    use crate::{
        config::InteractiveBrokersInstrumentProviderConfig,
        providers::instruments::InteractiveBrokersInstrumentProvider,
    };

    fn create_test_instrument_provider() -> InteractiveBrokersInstrumentProvider {
        let config = InteractiveBrokersInstrumentProviderConfig::default();
        InteractiveBrokersInstrumentProvider::new(config)
    }

    fn create_test_instrument_id() -> InstrumentId {
        InstrumentId::new(Symbol::from("AAPL"), Venue::from("NASDAQ"))
    }

    use rstest::rstest;

    #[rstest]
    fn test_ibalgo_with_zero_limit_price_maps_to_market() {
        assert_eq!(map_ib_order_type("IBALGO", Some(0.0)), OrderType::Market);
    }

    #[rstest]
    fn test_parse_execution_time_hyphenated_format() {
        let time_str = "20250225-15:15:00";
        let result = parse_execution_time(time_str);
        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert!(timestamp.as_i64() > 0);
    }

    #[rstest]
    fn test_parse_execution_time_with_met_timezone() {
        // Regression for European paper accounts that IB stamps with `MET`.
        // MET (CET) in February observes standard time (UTC+1).
        let met = parse_execution_time("20230223 00:43:36 MET").unwrap();
        let utc = parse_execution_time("20230223 00:43:36 Universal").unwrap();
        // Local 00:43:36 MET == 2023-02-22 23:43:36 UTC, i.e. 1 hour before UTC.
        assert_eq!(
            met.as_i64(),
            utc.as_i64() - 3_600_000_000_000,
            "MET (CET) should be 1h ahead of UTC in February"
        );
        assert!(met.as_i64() > 0);
    }

    #[rstest]
    fn test_parse_execution_time_applies_dst_for_regional_timezone() {
        // Same zone, two seasons: EST (UTC-5) in winter vs EDT (UTC-4) in summer.
        // Equal offsets would mean DST is NOT being applied - a real regression.
        let winter = parse_execution_time("20230223 00:43:36 America/New_York").unwrap();
        let summer = parse_execution_time("20230715 00:43:36 America/New_York").unwrap();
        let winter_utc = parse_execution_time("20230223 00:43:36 Universal").unwrap();
        let summer_utc = parse_execution_time("20230715 00:43:36 Universal").unwrap();
        assert_eq!(winter.as_i64(), winter_utc.as_i64() + 5 * 3_600_000_000_000); // EST
        assert_eq!(summer.as_i64(), summer_utc.as_i64() + 4 * 3_600_000_000_000); // EDT
    }

    #[rstest]
    fn test_parse_execution_time_dst_fall_back_fold_resolves_to_earliest() {
        // CME US/Central account (bebop23's case): on 2023-11-05 fall-back night
        // 01:30 America/Chicago occurs twice. Resolve to earliest (CDT, 06:30 UTC),
        // don't drop the fill.
        let fold = parse_execution_time("20231105 01:30:00 America/Chicago").unwrap();
        assert_eq!(
            fold.as_i64(),
            parse_execution_time("20231105 06:30:00 Universal")
                .unwrap()
                .as_i64()
        );
        assert_ne!(
            fold.as_i64(),
            parse_execution_time("20231105 07:30:00 Universal")
                .unwrap()
                .as_i64()
        );
    }

    #[rstest]
    fn test_parse_execution_time_dst_spring_forward_gap_errors() {
        // 02:30 America/Chicago never exists on 2023-03-12 spring-forward night.
        let gap = parse_execution_time("20230312 02:30:00 America/Chicago");
        assert!(gap.is_err());
    }

    #[rstest]
    fn test_parse_execution_time_fixed_offset_zone_without_dst() {
        // Asia/Tokyo is JST (UTC+9) year-round - guards the no-DST path.
        let tokyo = parse_execution_time("20230223 00:43:36 Asia/Tokyo").unwrap();
        let utc = parse_execution_time("20230223 00:43:36 Universal").unwrap();
        assert_eq!(tokyo.as_i64(), utc.as_i64() - 9 * 3_600_000_000_000);
    }

    #[rstest]
    fn test_parse_execution_time_with_unrecognised_timezone_errors() {
        let time_str = "20230223 00:43:36 Mars/Olympus";
        let result = parse_execution_time(time_str);
        assert!(result.is_err());
    }

    #[rstest]
    fn test_parse_execution_time_utc() {
        let time_str = "20230223 00:43:36 Universal";
        let result = parse_execution_time(time_str);
        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert!(timestamp.as_i64() > 0);
    }

    #[rstest]
    fn test_parse_execution_time_no_timezone_assumes_utc() {
        let time_str = "20230223 00:43:36";
        let result = parse_execution_time(time_str);
        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert!(timestamp.as_i64() > 0);
    }

    #[rstest]
    fn test_parse_execution_time_invalid_format() {
        let time_str = "invalid format";
        let result = parse_execution_time(time_str);
        assert!(result.is_err());
    }

    #[rstest]
    fn test_parse_execution_time_short_format() {
        let time_str = "20230223 00:43";
        let result = parse_execution_time(time_str);
        assert!(result.is_err());
    }

    #[rstest]
    fn test_parse_order_status_to_report_submitted() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let order_status = OrderStatus {
            order_id: 12345,
            status: OrderStatusKind::Submitted,
            filled: 0.0,
            remaining: 100.0,
            average_fill_price: Some(0.0),
            perm_id: 0,
            parent_id: 0,
            last_fill_price: Some(0.0),
            client_id: 0,
            why_held: String::new(),
            market_cap_price: Some(0.0),
        };

        let result = parse_order_status_to_report(
            &order_status,
            None,
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
        );

        // May fail if instrument not in provider, but that's expected
        if let Err(e) = result {
            let error_msg = e.to_string();
            assert!(
                error_msg.contains("not found") || error_msg.contains("instrument"),
                "Unexpected error: {}",
                error_msg
            );
        }
    }

    #[rstest]
    fn test_parse_order_status_to_report_filled() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let order_status = OrderStatus {
            order_id: 12345,
            status: OrderStatusKind::Filled,
            filled: 100.0,
            remaining: 0.0,
            average_fill_price: Some(150.25),
            perm_id: 0,
            parent_id: 0,
            last_fill_price: Some(150.25),
            client_id: 0,
            why_held: String::new(),
            market_cap_price: Some(0.0),
        };

        let result = parse_order_status_to_report(
            &order_status,
            None,
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
        );

        // May fail if instrument not in provider, but that's expected
        if let Err(e) = result {
            let error_msg = e.to_string();
            assert!(
                error_msg.contains("not found") || error_msg.contains("instrument"),
                "Unexpected error: {}",
                error_msg
            );
        }
    }

    #[rstest]
    fn test_parse_order_status_to_report_spread_allows_negative_avg_fill_price() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = InstrumentId::new(
            Symbol::from("(1)SPY C400_((1))SPY C410"),
            Venue::from("SMART"),
        );
        let account_id = AccountId::from("IB-001");

        let order_status = OrderStatus {
            order_id: 12345,
            status: OrderStatusKind::Filled,
            filled: 1.0,
            remaining: 0.0,
            average_fill_price: Some(-2.25),
            perm_id: 0,
            parent_id: 0,
            last_fill_price: Some(-2.25),
            client_id: 0,
            why_held: String::new(),
            market_cap_price: Some(0.0),
        };

        let report = parse_order_status_to_report(
            &order_status,
            None,
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
        )
        .unwrap();

        assert_eq!(report.avg_px, Some(Decimal::from_str("-2.25").unwrap()));
    }

    #[rstest]
    fn test_parse_order_status_to_report_known_raw_status_preserved() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let order_status = OrderStatus {
            order_id: 12345,
            status: OrderStatusKind::PendingSubmit,
            filled: 0.0,
            remaining: 100.0,
            average_fill_price: Some(0.0),
            perm_id: 0,
            parent_id: 0,
            last_fill_price: Some(0.0),
            client_id: 0,
            why_held: String::new(),
            market_cap_price: Some(0.0),
        };

        let report = parse_order_status_to_report(
            &order_status,
            None,
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
        )
        .unwrap();

        // A4: recognized raw vocabulary reaches the report byte-exact.
        assert_eq!(report.raw_order_status.as_deref(), Some("PendingSubmit"));
        assert_eq!(report.order_status, NautilusOrderStatus::Submitted);
    }

    #[rstest]
    fn test_parse_order_status_to_report_unknown_raw_status_preserved() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let order_status = OrderStatus {
            order_id: 12345,
            status: OrderStatusKind::Unknown("SomeFutureIbkrStatus".into()),
            filled: 0.0,
            remaining: 100.0,
            average_fill_price: Some(0.0),
            perm_id: 0,
            parent_id: 0,
            last_fill_price: Some(0.0),
            client_id: 0,
            why_held: String::new(),
            market_cap_price: Some(0.0),
        };

        let report = parse_order_status_to_report(
            &order_status,
            None,
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
        )
        .unwrap();

        // A5/A6: the unknown raw value reaches the report byte-exact while
        // the normalized status conservatively remains SUBMITTED - the
        // fallback does not erase provenance.
        assert_eq!(
            report.raw_order_status.as_deref(),
            Some("SomeFutureIbkrStatus"),
        );
        assert_eq!(report.order_status, NautilusOrderStatus::Submitted);
    }

    #[rstest]
    fn test_parse_order_status_to_report_inactive_maps_to_rejected() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let order_status = OrderStatus {
            order_id: 12345,
            status: OrderStatusKind::Inactive,
            filled: 0.0,
            remaining: 100.0,
            average_fill_price: Some(0.0),
            perm_id: 0,
            parent_id: 0,
            last_fill_price: Some(0.0),
            client_id: 0,
            why_held: String::new(),
            market_cap_price: Some(0.0),
        };

        let report = parse_order_status_to_report(
            &order_status,
            None,
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
        )
        .unwrap();

        assert_eq!(report.order_status, NautilusOrderStatus::Rejected);
    }

    #[rstest]
    fn test_parse_order_status_to_report_partial_fill_and_perm_fallback() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let order_status = OrderStatus {
            order_id: 0,
            status: OrderStatusKind::Submitted,
            filled: 3.0,
            remaining: 7.0,
            average_fill_price: Some(150.25),
            perm_id: 123_456,
            parent_id: 0,
            last_fill_price: Some(150.25),
            client_id: 0,
            why_held: String::new(),
            market_cap_price: Some(0.0),
        };
        let order = Order {
            action: Action::Buy,
            total_quantity: 10.0,
            order_type: "LMT".to_string(),
            limit_price: Some(150.25),
            order_ref: "O-20260527-001:123".to_string(),
            ..Default::default()
        };

        let report = parse_order_status_to_report(
            &order_status,
            Some(&order),
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
        )
        .unwrap();

        assert_eq!(report.order_status, NautilusOrderStatus::PartiallyFilled);
        assert_eq!(report.venue_order_id.to_string(), "PERM-123456");
        assert_eq!(
            report.client_order_id,
            Some(ClientOrderId::from("O-20260527-001"))
        );
    }

    #[rstest]
    fn test_ib_venue_order_id_prefers_perm_id_and_falls_back_to_order_id() {
        assert_eq!(ib_venue_order_id(123, 456).to_string(), "PERM-456");
        assert_eq!(ib_venue_order_id(123, 0).to_string(), "123");
    }

    #[rstest]
    fn test_normalized_order_ref_strips_ib_suffix() {
        assert_eq!(normalized_order_ref("O-001:123"), Some("O-001"));
        assert_eq!(normalized_order_ref("O-001"), Some("O-001"));
        assert_eq!(normalized_order_ref(""), None);
    }

    #[rstest]
    #[case(
        "MKT",
        None,
        None,
        None,
        None,
        OrderType::Market,
        None,
        None,
        None,
        None,
        None
    )]
    #[case(
        "LMT",
        Some(185.0),
        None,
        None,
        None,
        OrderType::Limit,
        Some(Price::new(185.0, 0)),
        None,
        None,
        None,
        None
    )]
    #[case(
        "IBALGO",
        Some(185.0),
        None,
        None,
        None,
        OrderType::Limit,
        Some(Price::new(185.0, 0)),
        None,
        None,
        None,
        None
    )]
    #[case(
        "IBALGO",
        None,
        None,
        None,
        None,
        OrderType::Market,
        None,
        None,
        None,
        None,
        None
    )]
    #[case(
        "MIT",
        None,
        Some(180.0),
        None,
        None,
        OrderType::MarketIfTouched,
        None,
        Some(Price::new(180.0, 0)),
        None,
        None,
        None
    )]
    #[case(
        "LIT",
        Some(179.0),
        Some(180.0),
        None,
        None,
        OrderType::LimitIfTouched,
        Some(Price::new(179.0, 0)),
        Some(Price::new(180.0, 0)),
        None,
        None,
        None
    )]
    #[case(
        "STP",
        None,
        Some(180.0),
        None,
        None,
        OrderType::StopMarket,
        None,
        Some(Price::new(180.0, 0)),
        None,
        None,
        None
    )]
    #[case(
        "STP LMT",
        Some(179.0),
        Some(180.0),
        None,
        None,
        OrderType::StopLimit,
        Some(Price::new(179.0, 0)),
        Some(Price::new(180.0, 0)),
        None,
        None,
        None
    )]
    #[case(
        "TRAIL LIMIT",
        None,
        Some(2.5),
        Some(185.0),
        Some(0.25),
        OrderType::TrailingStopLimit,
        None,
        Some(Price::new(185.0, 0)),
        Some(Decimal::from_str("0.25").unwrap()),
        Some(Decimal::from_str("2.5").unwrap()),
        Some(TrailingOffsetType::Price),
    )]
    fn test_parse_order_status_to_report_maps_pricing_fields_by_order_type(
        #[case] ib_order_type: &str,
        #[case] limit_price: Option<f64>,
        #[case] aux_price: Option<f64>,
        #[case] trail_stop_price: Option<f64>,
        #[case] limit_price_offset: Option<f64>,
        #[case] expected_order_type: OrderType,
        #[case] expected_price: Option<Price>,
        #[case] expected_trigger_price: Option<Price>,
        #[case] expected_limit_offset: Option<Decimal>,
        #[case] expected_trailing_offset: Option<Decimal>,
        #[case] expected_trailing_offset_type: Option<TrailingOffsetType>,
    ) {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let order_status = OrderStatus {
            order_id: 12345,
            status: OrderStatusKind::Submitted,
            filled: 0.0,
            remaining: 5.0,
            average_fill_price: Some(0.0),
            perm_id: 0,
            parent_id: 0,
            last_fill_price: Some(0.0),
            client_id: 0,
            why_held: String::new(),
            market_cap_price: Some(0.0),
        };

        let order = Order {
            action: Action::Buy,
            total_quantity: 5.0,
            order_type: ib_order_type.to_string(),
            limit_price,
            aux_price,
            trail_stop_price,
            limit_price_offset,
            tif: ibapi::orders::TimeInForce::GoodTilCanceled,
            ..Default::default()
        };

        let report = parse_order_status_to_report(
            &order_status,
            Some(&order),
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
        )
        .unwrap();

        assert_eq!(report.order_type, expected_order_type);
        assert_eq!(report.price, expected_price);
        assert_eq!(report.trigger_price, expected_trigger_price);
        assert_eq!(report.limit_offset, expected_limit_offset);
        assert_eq!(report.trailing_offset, expected_trailing_offset);
        assert_eq!(report.trailing_offset_type, expected_trailing_offset_type);
    }

    #[rstest]
    fn test_parse_order_status_to_report_maps_trailing_percent_to_basis_points() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let order_status = OrderStatus {
            order_id: 12345,
            status: OrderStatusKind::Submitted,
            filled: 0.0,
            remaining: 5.0,
            average_fill_price: Some(0.0),
            perm_id: 0,
            parent_id: 0,
            last_fill_price: Some(0.0),
            client_id: 0,
            why_held: String::new(),
            market_cap_price: Some(0.0),
        };

        let order = Order {
            action: Action::Buy,
            total_quantity: 5.0,
            order_type: "TRAIL".to_string(),
            trail_stop_price: Some(185.0),
            trailing_percent: Some(2.5),
            tif: ibapi::orders::TimeInForce::GoodTilCanceled,
            ..Default::default()
        };

        let report = parse_order_status_to_report(
            &order_status,
            Some(&order),
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
        )
        .unwrap();

        assert_eq!(report.order_type, OrderType::TrailingStopMarket);
        assert_eq!(report.trigger_price, Some(Price::new(185.0, 0)));
        assert_eq!(
            report.trailing_offset,
            Some(Decimal::from_str("250").unwrap())
        );
        assert_eq!(
            report.trailing_offset_type,
            Some(TrailingOffsetType::BasisPoints),
        );
        assert_eq!(report.limit_offset, None);
    }

    #[rstest]
    fn test_parse_execution_to_fill_report_buy() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let execution = Execution {
            order_id: 12345,
            client_id: 0,
            execution_id: String::from("EXEC-001"),
            time: String::from("20230223 00:43:36 Universal"),
            account_number: String::new(),
            exchange: String::new(),
            side: ExecutionSide::Bought,
            shares: 100.0,
            price: 150.25,
            perm_id: 0,
            liquidation: 0,
            cumulative_quantity: 100.0,
            average_price: 150.25,
            order_reference: String::from("ORDER-REF-001"),
            ev_rule: String::new(),
            ev_multiplier: None,
            model_code: String::new(),
            last_liquidity: Liquidity::None,
            pending_price_revision: false,
            submitter: String::new(),
        };

        let contract = Contract::default();
        let result = parse_execution_to_fill_report(
            &execution,
            &contract,
            1.0,
            "USD",
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
            None, // avg_px
        );

        // May fail if instrument not in provider, but that's expected
        match result {
            Err(e) => {
                let error_msg = e.to_string();
                assert!(
                    error_msg.contains("not found") || error_msg.contains("instrument"),
                    "Unexpected error: {}",
                    error_msg
                );
            }
            Ok(fill) => {
                assert_eq!(fill.order_side, OrderSide::Buy);
                assert_eq!(fill.trade_id.to_string(), "EXEC-001");
            }
        }
    }

    #[rstest]
    fn test_parse_execution_to_fill_report_clamps_only_pending_commission_sentinel() {
        let instrument_provider = create_test_instrument_provider();
        let instrument = equity_aapl();
        let instrument_id = instrument.id();
        instrument_provider.insert_test_instrument(InstrumentAny::from(instrument), 265598, 1);
        let account_id = AccountId::from("IB-001");
        let contract = Contract::default();

        for (commission, expected) in [(-1.0, 0.0), (-0.25, -0.25)] {
            let execution = Execution {
                order_id: 12345,
                client_id: 0,
                execution_id: format!("EXEC-{commission}"),
                time: String::from("20230223 00:43:36 Universal"),
                account_number: String::new(),
                exchange: String::new(),
                side: ExecutionSide::Bought,
                shares: 100.0,
                price: 150.25,
                perm_id: 0,
                liquidation: 0,
                cumulative_quantity: 100.0,
                average_price: 150.25,
                order_reference: String::from("ORDER-REF-001"),
                ev_rule: String::new(),
                ev_multiplier: None,
                model_code: String::new(),
                last_liquidity: Liquidity::None,
                pending_price_revision: false,
                submitter: String::new(),
            };

            let report = parse_execution_to_fill_report(
                &execution,
                &contract,
                commission,
                "USD",
                instrument_id,
                account_id,
                &instrument_provider,
                UnixNanos::new(0),
                None,
            )
            .unwrap();

            assert_eq!(report.commission, Money::new(expected, Currency::USD()));
        }
    }

    #[rstest]
    fn test_parse_execution_to_fill_report_sell() {
        let instrument_provider = create_test_instrument_provider();
        let instrument_id = create_test_instrument_id();
        let account_id = AccountId::from("IB-001");

        let execution = Execution {
            order_id: 12345,
            client_id: 0,
            execution_id: String::from("EXEC-002"),
            time: String::from("20230223 00:43:36 Universal"),
            account_number: String::new(),
            exchange: String::new(),
            side: ExecutionSide::Sold,
            shares: 50.0,
            price: 151.0,
            perm_id: 0,
            liquidation: 0,
            cumulative_quantity: 50.0,
            average_price: 151.0,
            order_reference: String::new(),
            ev_rule: String::new(),
            ev_multiplier: None,
            model_code: String::new(),
            last_liquidity: Liquidity::None,
            pending_price_revision: false,
            submitter: String::new(),
        };

        let contract = Contract::default();
        let result = parse_execution_to_fill_report(
            &execution,
            &contract,
            0.5,
            "USD",
            instrument_id,
            account_id,
            &instrument_provider,
            UnixNanos::new(0),
            None, // avg_px
        );

        // May fail if instrument not in provider, but that's expected
        match result {
            Err(e) => {
                let error_msg = e.to_string();
                assert!(
                    error_msg.contains("not found") || error_msg.contains("instrument"),
                    "Unexpected error: {}",
                    error_msg
                );
            }
            Ok(fill) => {
                assert_eq!(fill.order_side, OrderSide::Sell);
            }
        }
    }
}

#[cfg(test)]
mod completed_order_report_tests {
    use ibapi::{
        contracts::Contract,
        orders::{Action, Order, OrderData, OrderState, OrderStatusKind},
    };
    use nautilus_model::identifiers::{Symbol, Venue};
    use rstest::rstest;

    use super::*;
    use crate::{
        config::InteractiveBrokersInstrumentProviderConfig,
        providers::instruments::InteractiveBrokersInstrumentProvider,
    };

    const PERM_ID: i64 = 1377295418;
    const OTHER_PERM_ID: i64 = 1377295419;
    const CLIENT_ORDER_REF: &str = "O-C2-10P1-001";
    const OTHER_CLIENT_ORDER_REF: &str = "O-C2-10P1-002";
    const BROKER_ACCOUNT: &str = "DU1234567";
    const FOREIGN_BROKER_ACCOUNT: &str = "DU7654321";
    const TS_INIT: u64 = 1_756_000_000_000_000_000;

    fn instrument_provider() -> InteractiveBrokersInstrumentProvider {
        InteractiveBrokersInstrumentProvider::new(
            InteractiveBrokersInstrumentProviderConfig::default(),
        )
    }

    fn instrument_id() -> InstrumentId {
        InstrumentId::new(Symbol::from("AAPL"), Venue::from("NASDAQ"))
    }

    fn account_id() -> AccountId {
        AccountId::new("IB-DU1234567")
    }

    fn ts_init() -> UnixNanos {
        UnixNanos::from(TS_INIT)
    }

    fn order_with(perm_id: i64, filled_qty: f64, order_ref: &str, account: &str) -> Order {
        Order {
            order_id: 7,
            client_id: 1,
            perm_id,
            action: Action::Buy,
            total_quantity: 100.0,
            filled_quantity: filled_qty,
            order_type: "LMT".to_string(),
            limit_price: Some(25.0),
            account: account.to_string(),
            order_ref: order_ref.to_string(),
            ..Default::default()
        }
    }

    /// Mirrors how the vendored IB client decodes a `CompletedOrder` response:
    /// no live API order id (legacy `-1` sentinel) and the status carried on the
    /// order state.
    fn completed_order_data(status: OrderStatusKind, filled_qty: f64, perm_id: i64) -> OrderData {
        completed_order_data_with(
            status,
            filled_qty,
            perm_id,
            CLIENT_ORDER_REF,
            BROKER_ACCOUNT,
        )
    }

    fn completed_order_data_with(
        status: OrderStatusKind,
        filled_qty: f64,
        perm_id: i64,
        order_ref: &str,
        account: &str,
    ) -> OrderData {
        let order_state = OrderState {
            status,
            completed_status: "Cancelled by Trader".to_string(),
            completed_time: "20260910 15:30:00 America/New_York".to_string(),
            ..Default::default()
        };

        OrderData {
            order_id: -1,
            contract: Contract::default(),
            order: order_with(perm_id, filled_qty, order_ref, account),
            order_state,
        }
    }

    fn convert(data: &OrderData) -> anyhow::Result<OrderStatusReport> {
        parse_completed_order_to_report(
            data,
            instrument_id(),
            account_id(),
            &instrument_provider(),
            ts_init(),
        )
    }

    fn observation(report: OrderStatusReport, perm_id: i64) -> OrderReportObservation {
        OrderReportObservation {
            report,
            broker_perm_id: broker_perm_id(perm_id),
        }
    }

    fn open_order_report(
        status: OrderStatusKind,
        perm_id: i64,
        filled: f64,
        remaining: f64,
    ) -> OrderStatusReport {
        parse_order_status_to_report(
            &OrderStatus {
                order_id: 7,
                status,
                filled,
                remaining,
                average_fill_price: None,
                perm_id,
                parent_id: 0,
                last_fill_price: None,
                client_id: 1,
                why_held: String::new(),
                market_cap_price: None,
            },
            Some(&order_with(
                perm_id,
                filled,
                CLIENT_ORDER_REF,
                BROKER_ACCOUNT,
            )),
            instrument_id(),
            account_id(),
            &instrument_provider(),
            ts_init(),
        )
        .unwrap()
    }

    fn venue_order_id(perm_id: i64) -> VenueOrderId {
        VenueOrderId::new(format!("PERM-{perm_id}"))
    }

    // --------------------------------------------------------------------- //
    // Completed record -> report (identity, account, status)
    // --------------------------------------------------------------------- //

    #[rstest]
    fn test_completed_cancelled_record_becomes_order_status_report() {
        let report = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        assert_eq!(report.order_status, NautilusOrderStatus::Canceled);
        assert_eq!(report.quantity.as_f64(), 100.0);
        assert_eq!(report.filled_qty.as_f64(), 0.0);
    }

    #[rstest]
    fn test_completed_api_cancelled_record_maps_to_canceled() {
        let report = convert(&completed_order_data(
            OrderStatusKind::ApiCancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        assert_eq!(report.order_status, NautilusOrderStatus::Canceled);
    }

    #[rstest]
    fn test_completed_filled_record_maps_to_filled() {
        let report = convert(&completed_order_data(
            OrderStatusKind::Filled,
            100.0,
            PERM_ID,
        ))
        .unwrap();

        assert_eq!(report.order_status, NautilusOrderStatus::Filled);
        assert_eq!(report.filled_qty.as_f64(), 100.0);
    }

    #[rstest]
    fn test_completed_inactive_record_maps_to_rejected() {
        let report = convert(&completed_order_data(
            OrderStatusKind::Inactive,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        assert_eq!(report.order_status, NautilusOrderStatus::Rejected);
    }

    #[rstest]
    #[case(OrderStatusKind::Submitted)]
    #[case(OrderStatusKind::PreSubmitted)]
    #[case(OrderStatusKind::PendingCancel)]
    #[case(OrderStatusKind::PendingSubmit)]
    #[case(OrderStatusKind::ApiPending)]
    fn test_completed_non_terminal_status_fails_closed(#[case] status: OrderStatusKind) {
        let data = completed_order_data(status, 0.0, PERM_ID);

        assert!(convert(&data).is_err());
    }

    #[rstest]
    fn test_completed_unknown_raw_status_fails_closed() {
        let data = completed_order_data(
            OrderStatusKind::Unknown("SomeFutureIbkrStatus".into()),
            0.0,
            PERM_ID,
        );

        let error = convert(&data).unwrap_err();

        assert!(error.to_string().contains("SomeFutureIbkrStatus"));
    }

    #[rstest]
    fn test_cancelled_partial_fill_remains_terminal() {
        let report = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            40.0,
            PERM_ID,
        ))
        .unwrap();

        assert_eq!(report.order_status, NautilusOrderStatus::Canceled);
        assert_eq!(report.filled_qty.as_f64(), 40.0);
    }

    #[rstest]
    fn test_completed_order_venue_order_id_originates_from_broker_perm_id() {
        let report = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            OTHER_PERM_ID,
        ))
        .unwrap();

        assert_eq!(report.venue_order_id, venue_order_id(OTHER_PERM_ID));
    }

    #[rstest]
    #[case(1)]
    #[case(PERM_ID)]
    #[case(i64::MAX)]
    fn test_positive_perm_id_maps_to_perm_venue_order_id(#[case] perm_id: i64) {
        let report = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            perm_id,
        ))
        .unwrap();

        assert_eq!(
            report.venue_order_id,
            VenueOrderId::new(format!("PERM-{perm_id}"))
        );
    }

    #[rstest]
    fn test_broker_perm_id_accepts_only_strictly_positive_values() {
        assert_eq!(broker_perm_id(PERM_ID), Some(PERM_ID));
        assert_eq!(broker_perm_id(1), Some(1));
        assert_eq!(broker_perm_id(i64::MAX), Some(i64::MAX));

        assert_eq!(broker_perm_id(0), None);
        assert_eq!(broker_perm_id(-1), None);
        assert_eq!(broker_perm_id(-2), None);
        assert_eq!(broker_perm_id(i64::MIN), None);
    }

    #[rstest]
    fn test_completed_order_without_perm_id_fails_closed() {
        let data = completed_order_data(OrderStatusKind::Cancelled, 0.0, 0);

        assert!(convert(&data).is_err());
    }

    #[rstest]
    #[case(0)]
    #[case(-1)]
    #[case(-2)]
    #[case(i64::MIN)]
    fn test_completed_non_positive_perm_id_fails_closed(#[case] perm_id: i64) {
        let data = completed_order_data(OrderStatusKind::Cancelled, 0.0, perm_id);

        let error = convert(&data).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("is not a positive broker venue identity"),
            "the fail-closed error must name the invalid broker venue identity: {error}"
        );
    }

    #[rstest]
    fn test_completed_order_preserves_client_order_id() {
        let report = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        assert_eq!(
            report.client_order_id,
            Some(ClientOrderId::new(CLIENT_ORDER_REF))
        );
    }

    #[rstest]
    fn test_completed_order_client_order_id_ignores_strategy_suffix() {
        let data = completed_order_data_with(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
            "O-C2-10P1-001:leg-1",
            BROKER_ACCOUNT,
        );

        let report = convert(&data).unwrap();

        assert_eq!(
            report.client_order_id,
            Some(ClientOrderId::new(CLIENT_ORDER_REF))
        );
    }

    #[rstest]
    fn test_completed_order_with_empty_order_ref_fails_closed() {
        let data =
            completed_order_data_with(OrderStatusKind::Cancelled, 0.0, PERM_ID, "", BROKER_ACCOUNT);

        assert!(convert(&data).is_err());
    }

    #[rstest]
    fn test_completed_order_with_unusable_order_ref_fails_closed() {
        // Normalizing `":"` yields an empty base id; the record must be
        // rejected rather than panicking or emitting an empty client identity.
        let data = completed_order_data_with(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
            ":",
            BROKER_ACCOUNT,
        );

        assert!(convert(&data).is_err());
    }

    #[rstest]
    fn test_completed_order_with_matching_broker_account_is_accepted() {
        let report = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        assert_eq!(report.account_id, account_id());
        assert_eq!(report.instrument_id, instrument_id());
    }

    #[rstest]
    fn test_completed_order_with_empty_broker_account_fails_closed() {
        let data = completed_order_data_with(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
            CLIENT_ORDER_REF,
            "",
        );

        assert!(convert(&data).is_err());
    }

    #[rstest]
    fn test_completed_order_with_foreign_broker_account_fails_closed() {
        let data = completed_order_data_with(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
            CLIENT_ORDER_REF,
            FOREIGN_BROKER_ACCOUNT,
        );

        assert!(convert(&data).is_err());
    }

    #[rstest]
    fn test_completed_order_preserves_raw_order_status() {
        let report = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        assert_eq!(report.raw_order_status.as_deref(), Some("Cancelled"));
    }

    // --------------------------------------------------------------------- //
    // Open + completed merge (overlap, venue-identity transition)
    // --------------------------------------------------------------------- //

    #[rstest]
    fn test_empty_completed_response_preserves_open_order_reports() {
        let open = open_order_report(OrderStatusKind::Submitted, PERM_ID, 0.0, 100.0);

        let merged =
            merge_order_status_reports(vec![observation(open.clone(), PERM_ID)], Vec::new());

        assert_eq!(merged, vec![open]);
    }

    #[rstest]
    fn test_overlap_with_shared_perm_id_yields_one_completed_observation() {
        let open = open_order_report(OrderStatusKind::Submitted, PERM_ID, 0.0, 100.0);
        let completed = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        let merged = merge_order_status_reports(
            vec![observation(open, PERM_ID)],
            vec![observation(completed, PERM_ID)],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].order_status, NautilusOrderStatus::Canceled);
        assert_eq!(merged[0].venue_order_id, venue_order_id(PERM_ID));
        assert_eq!(
            merged[0].client_order_id,
            Some(ClientOrderId::new(CLIENT_ORDER_REF))
        );
    }

    #[rstest]
    fn test_overlap_precedence_and_ordering_are_deterministic() {
        let open = open_order_report(OrderStatusKind::Submitted, PERM_ID, 0.0, 100.0);
        let completed = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        let first = merge_order_status_reports(
            vec![observation(open.clone(), PERM_ID)],
            vec![observation(completed.clone(), PERM_ID)],
        );
        let second = merge_order_status_reports(
            vec![observation(open, PERM_ID)],
            vec![observation(completed, PERM_ID)],
        );

        assert_eq!(first, second);
    }

    #[rstest]
    fn test_venue_identity_transition_yields_one_completed_observation() {
        // The open observation has no broker venue identity (IBKR reports
        // perm_id == 0 until it acknowledges the order), so its venue order id is
        // only the API order-id fallback; the same order then appears completed
        // with a positive perm_id.
        let open = open_order_report(OrderStatusKind::Submitted, 0, 0.0, 100.0);
        assert_eq!(open.venue_order_id, VenueOrderId::new("7"));

        let completed = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        let merged = merge_order_status_reports(
            vec![observation(open, 0)],
            vec![observation(completed, PERM_ID)],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].order_status, NautilusOrderStatus::Canceled);
        assert_eq!(merged[0].venue_order_id, venue_order_id(PERM_ID));
        assert_eq!(
            merged[0].client_order_id,
            Some(ClientOrderId::new(CLIENT_ORDER_REF))
        );
    }

    #[rstest]
    fn test_non_positive_perm_id_is_not_authoritative_broker_venue_identity() {
        // A negative perm_id is invalid broker identity, so the observation holds
        // no authoritative venue identity and the correlated completed
        // observation replaces it with the real broker identity.
        let open = open_order_report(OrderStatusKind::Submitted, -1, 0.0, 100.0);
        assert_eq!(observation(open.clone(), -1).broker_perm_id, None);
        assert_eq!(observation(open.clone(), 0).broker_perm_id, None);

        let completed = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        let merged = merge_order_status_reports(
            vec![observation(open, -1)],
            vec![observation(completed, PERM_ID)],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].order_status, NautilusOrderStatus::Canceled);
        assert_eq!(merged[0].venue_order_id, venue_order_id(PERM_ID));
    }

    #[rstest]
    fn test_terminal_open_fallback_observation_is_upgraded_to_broker_venue_identity() {
        let open = open_order_report(OrderStatusKind::Filled, 0, 100.0, 0.0);
        assert_eq!(open.order_status, NautilusOrderStatus::Filled);
        assert_eq!(open.venue_order_id, VenueOrderId::new("7"));

        let completed = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        let merged = merge_order_status_reports(
            vec![observation(open, 0)],
            vec![observation(completed, PERM_ID)],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].order_status, NautilusOrderStatus::Canceled);
        assert_eq!(merged[0].venue_order_id, venue_order_id(PERM_ID));
    }

    #[rstest]
    fn test_venue_identity_transition_does_not_collapse_unrelated_orders() {
        // Same instrument, side, quantity and price as the completed record, but
        // a different client order identity: these are unrelated orders.
        let open = open_order_report(OrderStatusKind::Submitted, 0, 0.0, 100.0);
        let completed = convert(&completed_order_data_with(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
            OTHER_CLIENT_ORDER_REF,
            BROKER_ACCOUNT,
        ))
        .unwrap();

        let merged = merge_order_status_reports(
            vec![observation(open, 0)],
            vec![observation(completed, PERM_ID)],
        );

        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged[0].client_order_id,
            Some(ClientOrderId::new(CLIENT_ORDER_REF))
        );
        assert_eq!(
            merged[1].client_order_id,
            Some(ClientOrderId::new(OTHER_CLIENT_ORDER_REF))
        );
    }

    #[rstest]
    fn test_broker_identified_orders_are_not_collapsed_on_shared_client_identity() {
        // Two distinct broker venue orders sharing one client order identity are
        // kept distinct: collapsing them would hide a real broker order.
        let first = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();
        let second = convert(&completed_order_data(
            OrderStatusKind::Filled,
            100.0,
            OTHER_PERM_ID,
        ))
        .unwrap();

        let merged = merge_order_status_reports(
            Vec::new(),
            vec![
                observation(first, PERM_ID),
                observation(second, OTHER_PERM_ID),
            ],
        );

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].venue_order_id, venue_order_id(PERM_ID));
        assert_eq!(merged[1].venue_order_id, venue_order_id(OTHER_PERM_ID));
    }

    #[rstest]
    fn test_distinct_broker_orders_with_similar_attributes_remain_distinct() {
        let first = convert(&completed_order_data_with(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
            CLIENT_ORDER_REF,
            BROKER_ACCOUNT,
        ))
        .unwrap();
        let second = convert(&completed_order_data_with(
            OrderStatusKind::Cancelled,
            0.0,
            OTHER_PERM_ID,
            OTHER_CLIENT_ORDER_REF,
            BROKER_ACCOUNT,
        ))
        .unwrap();

        assert_eq!(first.instrument_id, second.instrument_id);
        assert_eq!(first.quantity, second.quantity);

        let merged = merge_order_status_reports(
            Vec::new(),
            vec![
                observation(first, PERM_ID),
                observation(second, OTHER_PERM_ID),
            ],
        );

        assert_eq!(merged.len(), 2);
        assert_ne!(merged[0].venue_order_id, merged[1].venue_order_id);
    }

    #[rstest]
    fn test_completed_only_observation_appends_after_open_observations() {
        let open = open_order_report(OrderStatusKind::Submitted, OTHER_PERM_ID, 0.0, 100.0);
        let completed = convert(&completed_order_data(
            OrderStatusKind::Cancelled,
            0.0,
            PERM_ID,
        ))
        .unwrap();

        let merged = merge_order_status_reports(
            vec![observation(open.clone(), OTHER_PERM_ID)],
            vec![observation(completed.clone(), PERM_ID)],
        );

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], open);
        assert_eq!(merged[1], completed);
    }
}
