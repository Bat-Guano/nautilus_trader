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

//! CWR-01B native contract probe for inbound IBKR order-type fidelity.
//!
//! Proves, deterministically and with no broker connection, the fidelity
//! contract the correction must satisfy:
//!
//! 1. an unrecognized wire type never becomes a canonical order type — it is
//!    refused with the exact raw wire identifier named in the error context
//!    (R1, R5);
//! 2. a recognized IBKR type whose semantics Nautilus cannot truthfully express
//!    is refused rather than collapsed onto `Market`/`Limit` (R2, R5);
//! 3. every published report carries the exact wire identifier byte-for-byte in
//!    `raw_order_type`, and the canonical mapping for the supported types is
//!    unchanged (R1, R2, R6);
//! 4. the four consumer-distinguishable classes (exact / attribute-lossy /
//!    unsupported / unknown / malformed) stay distinguishable (R5);
//! 5. broker price fields that do not carry a real price are never attached as
//!    price state, while real stop and trail prices still attach (O1);
//! 6. the completed-order path inherits all of the above (R4).
//!
//! No broker connection is created, no order is submitted, modified or
//! cancelled, and no external egress is configured.
//!
//! Run with:
//!
//! ```text
//! cargo test -p nautilus-interactive-brokers --test cwr_01b_order_type_fidelity_contract -- --nocapture
//! ```

use std::str::FromStr;

use ibapi::orders::{Action, Order, OrderData, OrderState, OrderStatus, OrderStatusKind};
use nautilus_core::UnixNanos;
use nautilus_interactive_brokers::{
    common::enums::IbOrderType,
    config::InteractiveBrokersInstrumentProviderConfig,
    execution::parse::{parse_completed_order_to_report, parse_order_status_to_report},
    providers::instruments::InteractiveBrokersInstrumentProvider,
};
use nautilus_model::{
    enums::OrderType,
    identifiers::{AccountId, InstrumentId, Symbol, Venue},
    types::Price,
};
use rstest::rstest;

const RAW_ACCOUNT_ID: &str = "DU1234567";
const NAUTILUS_ACCOUNT_ID: &str = "IB-DU1234567";
const BROKER_PERM_ID: i64 = 1377295418;
const CLIENT_ORDER_REF: &str = "O-CWR01B-001";
const TS_INIT: u64 = 1_756_000_000_000_000_000;

/// Wire types with an exact canonical mapping.
const EXACT_WIRE_TYPES: [(&str, OrderType); 9] = [
    ("MKT", OrderType::Market),
    ("LMT", OrderType::Limit),
    ("STP", OrderType::StopMarket),
    ("STP LMT", OrderType::StopLimit),
    ("TRAIL", OrderType::TrailingStopMarket),
    ("TRAIL LIMIT", OrderType::TrailingStopLimit),
    ("MIT", OrderType::MarketIfTouched),
    ("LIT", OrderType::LimitIfTouched),
    ("MTL", OrderType::MarketToLimit),
];

/// Wire types whose canonical mapping is truthful but attribute-lossy.
const PARTIAL_WIRE_TYPES: [(&str, OrderType); 4] = [
    ("MOC", OrderType::Market),
    ("LOC", OrderType::Limit),
    ("MKT PRT", OrderType::Market),
    ("STP PRT", OrderType::StopMarket),
];

/// Recognized IBKR wire types with no truthful canonical counterpart.
const UNSUPPORTED_WIRE_TYPES: [&str; 12] = [
    "MIDPRICE",
    "PEG MKT",
    "PEG STK",
    "PEG MID",
    "PEG BENCH",
    "PEG BEST",
    "REL",
    "PASSV REL",
    "VOL",
    "BOX TOP",
    "REL + LMT",
    "REL + MKT",
];

/// Wire values outside the recognized vocabulary.
const UNKNOWN_WIRE_TYPES: [&str; 6] = [
    "XYZ",
    "AUCTIONLIMIT",
    "COMBOMARKET",
    "PEG FIX",
    "peg mid",
    "MKT ",
];

fn instrument_id() -> InstrumentId {
    InstrumentId::new(Symbol::from("AAPL"), Venue::from("NASDAQ"))
}

fn account_id() -> AccountId {
    AccountId::new(NAUTILUS_ACCOUNT_ID)
}

fn instrument_provider() -> InteractiveBrokersInstrumentProvider {
    InteractiveBrokersInstrumentProvider::new(InteractiveBrokersInstrumentProviderConfig::default())
}

fn ts_init() -> UnixNanos {
    UnixNanos::from(TS_INIT)
}

fn order_status() -> OrderStatus {
    OrderStatus {
        order_id: 12345,
        status: OrderStatusKind::Submitted,
        filled: 0.0,
        remaining: 5.0,
        average_fill_price: Some(0.0),
        perm_id: BROKER_PERM_ID,
        parent_id: 0,
        last_fill_price: Some(0.0),
        client_id: 1,
        why_held: String::new(),
        market_cap_price: Some(0.0),
    }
}

fn ib_order(order_type: &str, limit_price: Option<f64>, aux_price: Option<f64>) -> Order {
    Order {
        action: Action::Buy,
        total_quantity: 5.0,
        order_type: order_type.to_string(),
        limit_price,
        aux_price,
        order_ref: CLIENT_ORDER_REF.to_string(),
        ..Default::default()
    }
}

fn parse(order: &Order) -> anyhow::Result<nautilus_model::reports::OrderStatusReport> {
    parse_order_status_to_report(
        &order_status(),
        Some(order),
        instrument_id(),
        account_id(),
        &instrument_provider(),
        ts_init(),
    )
}

fn completed_order_data(order_type: &str) -> OrderData {
    OrderData {
        order_id: -1,
        order: Order {
            order_id: -1,
            client_id: 1,
            perm_id: BROKER_PERM_ID,
            action: Action::Buy,
            total_quantity: 5.0,
            filled_quantity: 5.0,
            account: RAW_ACCOUNT_ID.to_string(),
            order_ref: CLIENT_ORDER_REF.to_string(),
            order_type: order_type.to_string(),
            ..Default::default()
        },
        order_state: OrderState {
            status: OrderStatusKind::Filled,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// R1/R5: an unrecognized wire type must never become `Market`, `Limit` or any
/// other canonical type, and the outcome must name the exact raw identifier.
#[rstest]
#[case("XYZ")]
#[case("AUCTIONLIMIT")]
#[case("COMBOMARKET")]
#[case("PEG FIX")]
#[case("peg mid")]
#[case("MKT ")]
fn test_01_unknown_wire_types_never_become_a_canonical_type(#[case] wire: &str) {
    let error = parse(&ib_order(wire, Some(185.0), Some(180.0)))
        .expect_err("an unrecognized wire type must never produce a report");

    let message = error.to_string();

    assert!(message.contains("unknown"), "{message}");
    assert!(
        message.contains(&format!("{wire:?}")),
        "the raw identifier must be named: {message}",
    );
}

/// R2/R5: recognized IBKR types with no truthful canonical counterpart are
/// refused instead of being collapsed onto a generic canonical type.
#[rstest]
fn test_02_unrepresentable_known_wire_types_are_refused() {
    for wire in UNSUPPORTED_WIRE_TYPES {
        let error = parse(&ib_order(wire, Some(185.0), Some(180.0))).expect_err(wire);
        let message = error.to_string();

        assert!(message.contains("unsupported"), "{wire}: {message}");
        assert!(message.contains(&format!("{wire:?}")), "{wire}: {message}");
    }
}

/// R5: an empty or whitespace-only wire value is malformed, and that is
/// distinguishable from an unknown value.
#[rstest]
#[case("")]
#[case("   ")]
fn test_03_malformed_wire_values_are_refused_and_distinguishable(#[case] wire: &str) {
    let error = parse(&ib_order(wire, Some(185.0), None)).expect_err(wire);
    let message = error.to_string();

    assert!(message.contains("malformed"), "{wire:?}: {message}");

    let unknown = parse(&ib_order("XYZ", Some(185.0), None))
        .expect_err("unknown")
        .to_string();

    assert!(unknown.contains("unknown"), "{unknown}");
    assert!(!unknown.contains("malformed"), "{unknown}");
}

/// R1/R2/R6: published reports keep the canonical mapping the adapter already
/// supported and carry the exact wire identifier byte-for-byte.
#[rstest]
fn test_04_published_reports_preserve_canonical_mapping_and_raw_identity() {
    for (wire, expected) in EXACT_WIRE_TYPES
        .into_iter()
        .chain(PARTIAL_WIRE_TYPES)
        .chain([("IBALGO", OrderType::Limit)])
    {
        let report = parse(&ib_order(wire, Some(185.0), None)).expect(wire);

        assert_eq!(report.order_type, expected, "{wire}");
        assert_eq!(
            report.raw_order_type.as_deref(),
            Some(wire),
            "{wire} must survive byte-for-byte",
        );
        assert_eq!(report.quantity.as_decimal().to_string(), "5");
    }
}

/// R5: a consumer can tell an exact projection from an attribute-lossy one
/// without widening the `OrderType` enum: the raw identifier either is the
/// canonical type's own wire string or it is not.
#[rstest]
fn test_05_exact_and_attribute_lossy_projections_stay_distinguishable() {
    let exact_wire = "MKT";
    let report = parse(&ib_order(exact_wire, None, None)).unwrap();
    let exact_type = IbOrderType::from_str(report.raw_order_type.as_deref().unwrap()).unwrap();

    assert!(exact_type.has_exact_canonical_projection());
    assert_eq!(
        exact_type.as_str(),
        report.raw_order_type.as_deref().unwrap()
    );
    assert_eq!(exact_type.nautilus_order_type(), Some(report.order_type));

    for (wire, _) in PARTIAL_WIRE_TYPES {
        let report = parse(&ib_order(wire, Some(185.0), None)).unwrap();
        let raw = report.raw_order_type.as_deref().unwrap();
        let known = IbOrderType::from_str(raw).unwrap();

        assert!(!known.has_exact_canonical_projection(), "{wire}");
        assert_eq!(known.nautilus_order_type(), Some(report.order_type));
        assert_ne!(
            raw,
            IbOrderType::from_str(exact_wire).unwrap().as_str(),
            "{wire} must not be indistinguishable from {exact_wire}",
        );
    }
}

/// Wire types whose `limit_price`/`aux_price` fields are prices (the trailing
/// types take a trailing *offset* in `aux_price`, handled separately).
const PRICE_FIELD_WIRE_TYPES: [&str; 11] = [
    "MKT", "LMT", "STP", "STP LMT", "MIT", "LIT", "MTL", "MOC", "LOC", "MKT PRT", "STP PRT",
];

/// O1: IBKR price sentinels (zero, negative, NaN, infinite) must never be
/// attached to a report, for every wire type that reaches the price derivation.
#[rstest]
fn test_06_unusable_prices_are_never_attached() {
    for unusable in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for wire in PRICE_FIELD_WIRE_TYPES {
            let report = parse(&ib_order(wire, Some(unusable), Some(unusable))).expect(wire);

            assert!(report.price.is_none(), "{wire} limit_price={unusable}");
            assert!(
                report.trigger_price.is_none(),
                "{wire} aux_price={unusable}",
            );
        }

        // The trailing types take their trigger price from `trail_stop_price`.
        for wire in ["TRAIL", "TRAIL LIMIT"] {
            let mut order = ib_order(wire, None, None);
            order.trail_stop_price = Some(unusable);
            let report = parse(&order).expect(wire);

            assert!(
                report.trigger_price.is_none(),
                "{wire} trail_stop_price={unusable}",
            );
        }
    }

    // Out of scope by design (O1b-3): `aux_price` is a trailing *offset* for the
    // trailing types, not a price. A non-convertible offset keeps its existing
    // explicit error instead of being silently fabricated.
    let mut trailing = ib_order("TRAIL", None, None);
    trailing.aux_price = Some(f64::NAN);
    assert!(parse(&trailing).is_err());
}

/// O1 non-regression: real stop, stop-limit and trailing prices still attach.
#[rstest]
fn test_07_real_stop_and_trail_prices_still_attach() {
    let stop = parse(&ib_order("STP", None, Some(180.0))).unwrap();
    assert_eq!(stop.trigger_price, Some(Price::new(180.0, 0)));

    let stop_limit = parse(&ib_order("STP LMT", Some(179.0), Some(180.0))).unwrap();
    assert_eq!(stop_limit.price, Some(Price::new(179.0, 0)));
    assert_eq!(stop_limit.trigger_price, Some(Price::new(180.0, 0)));

    let mut trailing = ib_order("TRAIL", None, None);
    trailing.trail_stop_price = Some(185.0);
    let trailing = parse(&trailing).unwrap();
    assert_eq!(trailing.trigger_price, Some(Price::new(185.0, 0)));
}

/// R4: the completed-order path (terminal broker evidence) inherits the same
/// fidelity guarantees.
#[rstest]
fn test_08_completed_order_path_inherits_type_fidelity() {
    let report = parse_completed_order_to_report(
        &completed_order_data("MKT PRT"),
        instrument_id(),
        account_id(),
        &instrument_provider(),
        ts_init(),
    )
    .unwrap();

    assert_eq!(report.order_type, OrderType::Market);
    assert_eq!(report.raw_order_type.as_deref(), Some("MKT PRT"));

    for wire in ["PEG MID", "XYZ", ""] {
        let error = parse_completed_order_to_report(
            &completed_order_data(wire),
            instrument_id(),
            account_id(),
            &instrument_provider(),
            ts_init(),
        )
        .expect_err(wire);

        assert!(
            error.to_string().contains(&format!("{wire:?}")),
            "{wire}: {error}",
        );
    }
}

/// R3: an unknown inbound broker order cannot be reconstructed as a market
/// order. The parser refuses, so no report exists for the engine's
/// external-order materialiser (`engine/mod.rs:1170` → `OrderAny::from_events`)
/// to turn into a `MarketOrder`; the submission path is not touched by this
/// change.
#[rstest]
fn test_09_refused_orders_cannot_be_materialised_as_market_orders() {
    for wire in UNSUPPORTED_WIRE_TYPES
        .iter()
        .copied()
        .chain(UNKNOWN_WIRE_TYPES)
    {
        assert!(
            parse(&ib_order(wire, Some(185.0), None)).is_err(),
            "{wire} produced a report that the engine could materialise",
        );
    }

    // A `Market` report is only ever produced by a wire type whose canonical
    // claim actually is a market order.
    let market_report = parse(&ib_order("MKT", None, None)).unwrap();
    assert_eq!(market_report.order_type, OrderType::Market);
    assert_eq!(market_report.raw_order_type.as_deref(), Some("MKT"));
}
