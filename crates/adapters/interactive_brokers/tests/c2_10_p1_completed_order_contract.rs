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

//! C2.10-P1 native contract probe for IBKR completed-order reconciliation.
//!
//! Proves, deterministically and with no broker connection, that an IBKR
//! *completed-order* record becomes a genuine
//! [`nautilus_model::reports::OrderStatusReport`] and that the produced report
//! travels the existing report-publication seam
//! (`reconciliation.raw.OrderStatusReport`, the topic the C2.8 delivery path
//! consumes).
//!
//! The probe builds real `ibapi::orders::OrderData` completed-order records (the
//! exact type the vendored IB client returns from `completed_orders`), converts
//! them through the adapter's production conversion path, and asserts the
//! invariants that make the report safe to publish:
//!
//! 1. a cancelled completed record becomes a terminal `CANCELED` report;
//! 2. terminal broker evidence is not downgraded by the working-order
//!    partial-fill rule;
//! 3. `client_order_id`, broker-derived `venue_order_id`, account identity,
//!    instrument identity and the raw broker status are preserved exactly;
//! 4. account provenance must be explicit broker evidence: a matching broker
//!    account is accepted, an empty or foreign broker account is rejected;
//! 5. client order identity must come from broker evidence: a valid `order_ref`
//!    is preserved exactly, an empty or unusable one is rejected;
//! 6. only terminal statuses are accepted from the completed-order source;
//!    known non-terminal and unknown raw statuses are rejected;
//! 7. broker venue identity requires a strictly positive `perm_id`: zero and
//!    negative values are rejected and can never become a `PERM-{perm_id}` venue
//!    id;
//! 8. open/completed overlap produces one deterministic observation, including
//!    across the venue-identity transition (open observation without a usable
//!    `perm_id`, completed observation with a positive `perm_id`);
//! 9. unrelated orders with superficially similar attributes remain distinct;
//! 10. an empty completed response leaves open-order observations unchanged;
//! 11. the report is delivered on the real reconciliation report topic unchanged.
//!
//! No broker connection is created, no order is submitted or cancelled, and no
//! external egress is configured (the probe never installs an external egress
//! backend, so report publication cannot leave the process).
//!
//! Run with:
//!
//! ```text
//! cargo test -p nautilus-interactive-brokers --test c2_10_p1_completed_order_contract -- --nocapture
//! ```

use std::{cell::RefCell, rc::Rc};

use ibapi::{
    contracts::Contract,
    orders::{Action, Order, OrderData, OrderState, OrderStatus as IBOrderStatus, OrderStatusKind},
};
use nautilus_common::msgbus::{
    MStr, MessageBus, MessagingSwitchboard, Pattern, ShareableMessageHandler, publish_any,
    set_message_bus, subscribe_any,
};
use nautilus_core::UnixNanos;
use nautilus_interactive_brokers::{
    config::InteractiveBrokersInstrumentProviderConfig,
    execution::parse::{
        OrderReportObservation, broker_perm_id, merge_order_status_reports,
        parse_completed_order_to_report, parse_order_status_to_report,
    },
    providers::instruments::InteractiveBrokersInstrumentProvider,
};
use nautilus_model::{
    enums::OrderStatus as NautilusOrderStatus,
    identifiers::{AccountId, ClientOrderId, InstrumentId, Symbol, Venue, VenueOrderId},
    reports::OrderStatusReport,
};
use rstest::rstest;

const CONFIRMATION: &str = "C2.10-P1 COMPLETED ORDER REPORT CONTRACT CONFIRMED";
const RAW_ACCOUNT_ID: &str = "DU1234567";
const FOREIGN_RAW_ACCOUNT_ID: &str = "DU7654321";
const NAUTILUS_ACCOUNT_ID: &str = "IB-DU1234567";
const BROKER_PERM_ID: i64 = 1377295418;
const OTHER_BROKER_PERM_ID: i64 = 1377295419;
const CLIENT_ORDER_REF: &str = "O-C2-10P1-001";
const OTHER_CLIENT_ORDER_REF: &str = "O-C2-10P1-002";
const TS_INIT: u64 = 1_756_000_000_000_000_000;

fn instrument_id() -> InstrumentId {
    InstrumentId::new(Symbol::from("AAPL"), Venue::from("NASDAQ"))
}

fn account_id() -> AccountId {
    AccountId::new(NAUTILUS_ACCOUNT_ID)
}

fn ts_init() -> UnixNanos {
    UnixNanos::from(TS_INIT)
}

fn instrument_provider() -> InteractiveBrokersInstrumentProvider {
    InteractiveBrokersInstrumentProvider::new(InteractiveBrokersInstrumentProviderConfig::default())
}

fn base_order(perm_id: i64, filled_qty: f64, order_ref: &str, account: &str) -> Order {
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

/// A completed-order record shaped exactly like the vendored client's
/// `CompletedOrder` decoding: no live API order id (legacy `-1` sentinel), the
/// broker `perm_id`, and an order state carrying the raw status.
fn completed_order_data(
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
        order: base_order(perm_id, filled_qty, order_ref, account),
        order_state,
    }
}

fn completed(
    status: OrderStatusKind,
    filled_qty: f64,
    perm_id: i64,
) -> anyhow::Result<OrderStatusReport> {
    convert(&completed_order_data(
        status,
        filled_qty,
        perm_id,
        CLIENT_ORDER_REF,
        RAW_ACCOUNT_ID,
    ))
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
    order_ref: &str,
) -> OrderStatusReport {
    parse_order_status_to_report(
        &IBOrderStatus {
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
        Some(&base_order(perm_id, filled, order_ref, RAW_ACCOUNT_ID)),
        instrument_id(),
        account_id(),
        &instrument_provider(),
        ts_init(),
    )
    .expect("open order report must convert")
}

/// Publishes a report on the real reconciliation report topic and captures what
/// subscribers received. No external egress backend is installed.
fn publish_and_capture(report: &OrderStatusReport) -> Vec<OrderStatusReport> {
    set_message_bus(Rc::new(RefCell::new(MessageBus::default())));

    let captured: Rc<RefCell<Vec<OrderStatusReport>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = captured.clone();
    let handler = ShareableMessageHandler::from_typed(move |message: &OrderStatusReport| {
        sink.borrow_mut().push(message.clone());
    });

    let topic = MessagingSwitchboard::reconciliation_raw_order_status_report_topic();
    let pattern: MStr<Pattern> = topic.into();
    subscribe_any(pattern, handler, None);
    publish_any(topic, report);

    captured.borrow().clone()
}

#[rstest]
fn c2_10_p1_completed_order_report_contract() {
    // 1. A completed/cancelled broker record becomes a genuine OrderStatusReport
    //    with terminal status, exact identities and preserved raw provenance.
    let report = completed(OrderStatusKind::Cancelled, 0.0, BROKER_PERM_ID)
        .expect("completed order must convert into an OrderStatusReport");

    assert_eq!(
        report.order_status,
        NautilusOrderStatus::Canceled,
        "terminal status must come from the completed-order record"
    );
    assert_eq!(
        report.raw_order_status.as_deref(),
        Some("Cancelled"),
        "raw broker status must be preserved"
    );
    assert_eq!(
        report.client_order_id,
        Some(ClientOrderId::new(CLIENT_ORDER_REF)),
        "client_order_id must be preserved exactly"
    );
    assert_eq!(
        report.venue_order_id,
        VenueOrderId::new(format!("PERM-{BROKER_PERM_ID}")),
        "venue identity must originate from the broker perm_id"
    );
    assert_eq!(
        report.account_id,
        account_id(),
        "Nautilus account identity must be preserved"
    );
    assert_eq!(
        report.instrument_id,
        instrument_id(),
        "instrument identity must be preserved"
    );
    assert_eq!(
        report.quantity.as_f64(),
        100.0,
        "quantity must be preserved"
    );
    assert_eq!(
        report.filled_qty.as_f64(),
        0.0,
        "filled quantity must be preserved"
    );

    // 2. Terminal broker evidence survives a partial fill.
    let partial_report =
        completed(OrderStatusKind::Cancelled, 40.0, BROKER_PERM_ID).expect("partial fill converts");
    assert_eq!(
        partial_report.order_status,
        NautilusOrderStatus::Canceled,
        "a cancelled order stays terminal even after a partial fill"
    );
    assert_eq!(partial_report.filled_qty.as_f64(), 40.0);

    // 3. Account provenance must be explicit broker evidence.
    let empty_account = convert(&completed_order_data(
        OrderStatusKind::Cancelled,
        0.0,
        BROKER_PERM_ID,
        CLIENT_ORDER_REF,
        "",
    ));
    assert!(
        empty_account.is_err(),
        "an empty broker account must not be replaced by the local account"
    );
    let foreign_account = convert(&completed_order_data(
        OrderStatusKind::Cancelled,
        0.0,
        BROKER_PERM_ID,
        CLIENT_ORDER_REF,
        FOREIGN_RAW_ACCOUNT_ID,
    ));
    assert!(
        foreign_account.is_err(),
        "a foreign broker account must not be attributed to the local account"
    );

    // 4. Client order identity must come from broker evidence.
    let empty_order_ref = convert(&completed_order_data(
        OrderStatusKind::Cancelled,
        0.0,
        BROKER_PERM_ID,
        "",
        RAW_ACCOUNT_ID,
    ));
    assert!(
        empty_order_ref.is_err(),
        "an empty order_ref must fail closed before publication"
    );
    let unusable_order_ref = convert(&completed_order_data(
        OrderStatusKind::Cancelled,
        0.0,
        BROKER_PERM_ID,
        ":",
        RAW_ACCOUNT_ID,
    ));
    assert!(
        unusable_order_ref.is_err(),
        "an unusable order_ref must fail closed before publication"
    );

    // 5. Completed-order authority is terminal-only.
    for accepted in [
        OrderStatusKind::Cancelled,
        OrderStatusKind::ApiCancelled,
        OrderStatusKind::Filled,
        OrderStatusKind::Inactive,
    ] {
        assert!(
            completed(accepted, 0.0, BROKER_PERM_ID).is_ok(),
            "terminal statuses must be accepted by the completed-order source"
        );
    }

    for rejected in [
        OrderStatusKind::Submitted,
        OrderStatusKind::PreSubmitted,
        OrderStatusKind::PendingCancel,
        OrderStatusKind::PendingSubmit,
        OrderStatusKind::ApiPending,
    ] {
        assert!(
            completed(rejected, 0.0, BROKER_PERM_ID).is_err(),
            "known non-terminal statuses must be rejected from completed-order authority"
        );
    }
    let unknown_status = completed(
        OrderStatusKind::Unknown("SomeFutureIbkrStatus".into()),
        0.0,
        BROKER_PERM_ID,
    )
    .expect_err("unknown raw status must fail closed");
    assert!(
        unknown_status.to_string().contains("SomeFutureIbkrStatus"),
        "the fail-closed error must name the unmappable raw status"
    );

    // 6. Broker venue identity must be strictly positive: a zero or negative
    //    `perm_id` is not authoritative and cannot produce a completed-order
    //    report, so it can never become a `PERM-{perm_id}` venue id.
    for invalid_perm_id in [0, -1, -2, i64::MIN] {
        let error = completed(OrderStatusKind::Cancelled, 0.0, invalid_perm_id)
            .expect_err("a completed record with a non-positive perm_id must fail closed");
        assert!(
            error
                .to_string()
                .contains("is not a positive broker venue identity"),
            "the fail-closed error must name the invalid broker venue identity"
        );
    }
    assert_eq!(
        broker_perm_id(0),
        None,
        "zero is not authoritative broker venue identity"
    );
    assert_eq!(
        broker_perm_id(-1),
        None,
        "a negative perm_id is not authoritative broker venue identity"
    );
    assert_eq!(
        broker_perm_id(i64::MIN),
        None,
        "a negative perm_id is not authoritative broker venue identity"
    );
    let positive_report = completed(OrderStatusKind::Cancelled, 0.0, BROKER_PERM_ID)
        .expect("a strictly positive perm_id remains authoritative");
    assert_eq!(
        positive_report.venue_order_id,
        VenueOrderId::new(format!("PERM-{BROKER_PERM_ID}")),
        "a positive perm_id still maps to the PERM- venue id form"
    );

    // 7. Overlap with a shared positive perm_id yields one terminal observation.
    let shared_open = open_order_report(
        OrderStatusKind::Submitted,
        BROKER_PERM_ID,
        0.0,
        100.0,
        CLIENT_ORDER_REF,
    );
    assert!(
        !shared_open.order_status.is_closed(),
        "the open observation must be non-terminal for this proof"
    );
    let merged = merge_order_status_reports(
        vec![observation(shared_open.clone(), BROKER_PERM_ID)],
        vec![observation(report.clone(), BROKER_PERM_ID)],
    );
    assert_eq!(
        merged.len(),
        1,
        "one observation per correlated broker order"
    );
    assert_eq!(
        merged[0].order_status,
        NautilusOrderStatus::Canceled,
        "terminal broker evidence must take precedence"
    );
    let merged_again = merge_order_status_reports(
        vec![observation(shared_open, BROKER_PERM_ID)],
        vec![observation(report.clone(), BROKER_PERM_ID)],
    );
    assert_eq!(
        merged, merged_again,
        "overlap resolution must be deterministic"
    );

    // 8. Venue-identity transition: the open observation has no usable perm_id,
    //    so its venue id is only the API order-id fallback; the same order then
    //    completes with a positive perm_id.
    let fallback_open =
        open_order_report(OrderStatusKind::Submitted, 0, 0.0, 100.0, CLIENT_ORDER_REF);
    assert_eq!(fallback_open.venue_order_id, VenueOrderId::new("7"));
    let transition = merge_order_status_reports(
        vec![observation(fallback_open, 0)],
        vec![observation(report.clone(), BROKER_PERM_ID)],
    );
    assert_eq!(
        transition.len(),
        1,
        "the venue-identity transition must still yield one coherent observation"
    );
    assert_eq!(transition[0].order_status, NautilusOrderStatus::Canceled);
    assert_eq!(
        transition[0].venue_order_id,
        VenueOrderId::new(format!("PERM-{BROKER_PERM_ID}")),
        "the merged observation must carry broker venue identity"
    );

    // 9. Unrelated orders with superficially similar attributes remain distinct.
    let unrelated_open =
        open_order_report(OrderStatusKind::Submitted, 0, 0.0, 100.0, CLIENT_ORDER_REF);
    let unrelated_completed = convert(&completed_order_data(
        OrderStatusKind::Cancelled,
        0.0,
        BROKER_PERM_ID,
        OTHER_CLIENT_ORDER_REF,
        RAW_ACCOUNT_ID,
    ))
    .expect("second client order must convert");
    let distinct = merge_order_status_reports(
        vec![observation(unrelated_open, 0)],
        vec![observation(unrelated_completed, BROKER_PERM_ID)],
    );
    assert_eq!(
        distinct.len(),
        2,
        "unrelated orders must not collapse on similar attributes"
    );

    let other_perm = completed(OrderStatusKind::Filled, 100.0, OTHER_BROKER_PERM_ID)
        .expect("second broker order must convert");
    let two_broker_orders = merge_order_status_reports(
        Vec::new(),
        vec![
            observation(report.clone(), BROKER_PERM_ID),
            observation(other_perm, OTHER_BROKER_PERM_ID),
        ],
    );
    assert_eq!(
        two_broker_orders.len(),
        2,
        "distinct broker venue orders must remain distinct"
    );
    assert_ne!(
        two_broker_orders[0].venue_order_id, two_broker_orders[1].venue_order_id,
        "distinct orders must carry distinct venue identities"
    );

    // 10. An empty completed-order response leaves open-order observations
    //     unchanged (no terminal state is inferred from absence).
    let open_only = merge_order_status_reports(
        vec![observation(
            open_order_report(
                OrderStatusKind::Submitted,
                BROKER_PERM_ID,
                0.0,
                100.0,
                CLIENT_ORDER_REF,
            ),
            BROKER_PERM_ID,
        )],
        Vec::new(),
    );
    assert_eq!(open_only.len(), 1);
    assert!(
        !open_only[0].order_status.is_closed(),
        "no completed evidence must not fabricate a terminal state"
    );

    // 11. The produced report crosses the existing report-publication seam
    //     (the C2.8 delivery topic) unchanged.
    let delivered = publish_and_capture(&report);
    assert_eq!(
        delivered.len(),
        1,
        "the report must be published exactly once on the reconciliation report topic"
    );
    assert_eq!(
        delivered[0], report,
        "the delivered report must be identical to the produced report"
    );

    println!("{CONFIRMATION}");
}
