use super::*;
use crate::common::test_utils::wire_enum::{check_wire_enum_rejects_unknown, check_wire_enum_round_trip};
use crate::ToField;
use std::str::FromStr;

const ALL_KINDS: &[(OrderStatusKind, &str)] = &[
    (OrderStatusKind::ApiPending, "ApiPending"),
    (OrderStatusKind::PendingSubmit, "PendingSubmit"),
    (OrderStatusKind::PendingCancel, "PendingCancel"),
    (OrderStatusKind::PreSubmitted, "PreSubmitted"),
    (OrderStatusKind::Submitted, "Submitted"),
    (OrderStatusKind::ApiCancelled, "ApiCancelled"),
    (OrderStatusKind::Cancelled, "Cancelled"),
    (OrderStatusKind::Filled, "Filled"),
    (OrderStatusKind::Inactive, "Inactive"),
];

#[test]
fn order_status_kind_round_trip() {
    // Inline equivalent of `check_wire_enum_round_trip`; the shared helper
    // requires `Copy`, which `OrderStatusKind` intentionally dropped for
    // `Unknown(String)` (C2.6).
    for (kind, wire) in ALL_KINDS {
        assert_eq!(kind.to_string(), *wire, "Display for {kind:?}");
        assert_eq!(OrderStatusKind::from_str(*wire).unwrap(), *kind, "FromStr({wire})");
        assert_eq!(kind.to_field(), *wire, "ToField for {kind:?}");
        assert_eq!(kind.as_str(), *wire, "as_str for {kind:?}");
    }
}

#[test]
fn order_status_kind_from_str_preserves_unknown_identity() {
    let kind: OrderStatusKind = "SomeFutureIbkrStatus".parse().unwrap();
    assert_eq!(kind, OrderStatusKind::Unknown("SomeFutureIbkrStatus".into()));
    assert_eq!(kind.as_str(), "SomeFutureIbkrStatus");
    assert_eq!(kind.to_string(), "SomeFutureIbkrStatus");
    assert_eq!(kind.to_field(), "SomeFutureIbkrStatus");
}

#[test]
fn order_status_kind_from_str_still_rejects_empty() {
    assert!(matches!(
        OrderStatusKind::from_str(""),
        Err(crate::Error::Parse(_, _, _))
    ));
}

#[test]
fn order_status_kind_unknown_is_neither_active_nor_terminal() {
    let kind = OrderStatusKind::Unknown("SomeFutureIbkrStatus".into());
    assert!(!kind.is_active());
    assert!(!kind.is_terminal());
}

#[test]
fn order_status_kind_unknown_serde_round_trip() {
    let kind = OrderStatusKind::Unknown("SomeFutureIbkrStatus".into());
    let json = serde_json::to_string(&kind).unwrap();
    assert_eq!(json, r#"{"Unknown":"SomeFutureIbkrStatus"}"#);
    let decoded: OrderStatusKind = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, kind);
}

#[test]
fn order_status_kind_known_serialization_unchanged() {
    for (kind, wire) in ALL_KINDS {
        let json = serde_json::to_string(kind).unwrap();
        assert_eq!(json, format!("\"{wire}\""));
    }
}

#[test]
fn execution_filter_side_round_trip() {
    check_wire_enum_round_trip(&[(ExecutionFilterSide::Buy, "BUY"), (ExecutionFilterSide::Sell, "SELL")]);
}

#[test]
fn execution_filter_side_from_str_rejects_unknown() {
    // Empty + arbitrary; case-sensitive (lowercase rejected); Action variants
    // (SSHORT/SLONG) not accepted on the filter; Execution.side wire (BOT/SLD)
    // also rejected — field-scoped vocabulary.
    check_wire_enum_rejects_unknown::<ExecutionFilterSide>(&["", "INVALID", "buy", "sell", "SSHORT", "SLONG", "BOT", "SLD"]);
}

#[test]
fn execution_side_round_trip() {
    check_wire_enum_round_trip(&[(ExecutionSide::Bought, "BOT"), (ExecutionSide::Sold, "SLD")]);
}

#[test]
fn execution_side_from_str_rejects_unknown() {
    // Empty + arbitrary; case-sensitive (lowercase rejected); ExecutionFilter
    // vocab (BUY/SELL) and Action vocab (SSHORT/SLONG) both rejected on the
    // execution-side field — field-scoped vocabulary per C# Execution.cs:83.
    check_wire_enum_rejects_unknown::<ExecutionSide>(&["", "INVALID", "bot", "sld", "BUY", "SELL", "SSHORT", "SLONG"]);
}

#[test]
fn is_active_and_is_terminal_partition_of_known_variants() {
    // Exhaustive check: exactly one helper returns true for 8 variants;
    // ApiPending is the documented gap (neither active nor terminal);
    // Unknown(String) is excluded here and covered by its own test.
    for (kind, text) in ALL_KINDS {
        let active = kind.is_active();
        let terminal = kind.is_terminal();
        match kind {
            OrderStatusKind::PreSubmitted | OrderStatusKind::PendingSubmit | OrderStatusKind::PendingCancel | OrderStatusKind::Submitted => {
                assert!(active, "{text} should be active");
                assert!(!terminal, "{text} should not be terminal");
            }
            OrderStatusKind::Filled | OrderStatusKind::Cancelled | OrderStatusKind::ApiCancelled | OrderStatusKind::Inactive => {
                assert!(!active, "{text} should not be active");
                assert!(terminal, "{text} should be terminal");
            }
            OrderStatusKind::ApiPending => {
                assert!(!active, "ApiPending should not be active");
                assert!(!terminal, "ApiPending should not be terminal");
            }
            OrderStatusKind::Unknown(_) => unreachable!("ALL_KINDS contains no Unknown"),
        }
    }
}
