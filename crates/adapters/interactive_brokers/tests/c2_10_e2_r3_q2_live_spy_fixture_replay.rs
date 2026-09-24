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

//! C2.10-E2-R3-Q2 offline replay of the EXACT live SPY contract-details fixture.
//!
//! This is the bridge that C2.10-E2-R2-R1 was missing.  The R2-R1 offline
//! demonstration could only start from a *hand-constructed* native `Equity`,
//! because an `ibapi::contracts::ContractDetails` cannot be built from Python -
//! so nobody ever joined the live `ContractDetails -> Equity` leg to the Python
//! legality gate.  When the live campaign finally ran, the two halves disagreed
//! and every SPY order was refused.
//!
//! Here the REAL production parser runs on the REAL live fixture, and its
//! ACTUAL output is written to a JSON file.  The Python side of the campaign
//! consumes that file rather than values typed by hand, so the transcription
//! between the two halves is the parser's own output and nothing else.
//!
//! Set `C210R3Q2_FIXTURE_OUT` to the output path.  With no variable set the
//! test still asserts everything and simply skips the file write, so it is safe
//! in an ordinary test run.

use std::path::PathBuf;

use ibapi::contracts::{Contract, ContractDetails, Currency, Exchange, SecurityType, Symbol};
use nautilus_core::Params;
use nautilus_interactive_brokers::providers::parse::parse_ib_contract_to_instrument;
use nautilus_model::{
    enums::AssetClass,
    identifiers::{InstrumentId, Symbol as NautilusSymbol, Venue},
    instruments::{Instrument, InstrumentAny},
    types::{Price, Quantity},
};

/// The exact `ContractDetails` the live TWS PAPER session returned for SPY.
///
/// Recorded in C2.10-E2-Q1-R1 from the live provider before this correction:
///
/// ```text
/// {'secType': 'STK', 'conId': 756733, 'exchange': 'SMART',
///  'primaryExchange': 'ARCA', 'symbol': 'SPY', 'localSymbol': 'SPY',
///  'currency': 'USD', 'tradingClass': 'SPY'}
/// minSize                = 0.0001
/// sizeIncrement          = 0.0001
/// suggestedSizeIncrement = 40.0
/// priceMagnifier         = 1
/// ```
fn live_spy_contract_details() -> ContractDetails {
    ContractDetails {
        contract: Contract {
            symbol: Symbol::from("SPY"),
            security_type: SecurityType::Stock,
            exchange: Exchange::from("SMART"),
            primary_exchange: Exchange::from("ARCA"),
            currency: Currency::from("USD"),
            local_symbol: "SPY".to_string(),
            ..Default::default()
        },
        min_tick: 0.01,
        min_size: 0.0001,
        size_increment: 0.0001,
        suggested_size_increment: 40.0,
        ..Default::default()
    }
}

fn info_string(info: &Option<Params>, key: &str) -> Option<String> {
    info.as_ref()
        .and_then(|params| params.get(key))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
}

#[test]
fn test_c2_10_e2_r3_q2_live_spy_fixture_replays_through_the_real_parser() {
    let details = live_spy_contract_details();
    let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));

    let instrument = parse_ib_contract_to_instrument(&details, instrument_id)
        .expect("the live SPY fixture must parse");
    let InstrumentAny::Equity(equity) = instrument else {
        panic!("the live SPY fixture must resolve to an Equity");
    };

    assert_eq!(equity.id, instrument_id);
    assert_eq!(equity.asset_class(), AssetClass::Equity);
    assert_eq!(equity.size_precision(), 0);

    // The published minimum is the normalization of the live fractional
    // constraint, and NOT the venue's 0.0001 rendered at precision 0.
    assert_eq!(
        equity.min_quantity(),
        Some(Quantity::new(1.0, 0)),
        "live SPY minSize 0.0001 / sizeIncrement 0.0001 must normalize to 1 share"
    );

    let info = equity.info.clone().expect("instrument info");
    assert_eq!(
        info_string(&Some(info.clone()), "minQuantityRawMinSize").as_deref(),
        Some("0.0001")
    );
    assert_eq!(
        info_string(&Some(info.clone()), "minQuantityRawSizeIncrement").as_deref(),
        Some("0.0001")
    );
    assert_eq!(
        info_string(&Some(info.clone()), "minQuantityEffective").as_deref(),
        Some("1")
    );
    assert_eq!(
        info_string(&Some(info.clone()), "minQuantitySystemIncrement").as_deref(),
        Some("1")
    );
    assert_eq!(
        info_string(&Some(info.clone()), "minQuantitySource").as_deref(),
        Some("contract_details.min_size.ceiling_to_system_size_increment")
    );
    assert_eq!(equity.lot_size(), Some(Quantity::new(100.0, 0)));
    assert_eq!(equity.price_precision(), 2);
    assert_eq!(equity.price_increment(), Price::new(0.01, 2));

    // --- hand the parser's OWN output to the Python half of the campaign --- //
    let Some(out_path) = std::env::var_os("C210R3Q2_FIXTURE_OUT").map(PathBuf::from) else {
        return;
    };

    let payload = serde_json::json!({
        "fixture": "c2_10_e2_r3_q2_live_spy_contract_details",
        "source": "live TWS PAPER provider response recorded in C2.10-E2-Q1-R1",
        "produced_by": "nautilus_interactive_brokers::providers::parse::parse_ib_contract_to_instrument",
        "contract": {
            "symbol": "SPY",
            "security_type": "STK",
            "exchange": "SMART",
            "primary_exchange": "ARCA",
            "local_symbol": "SPY",
            "currency": "USD",
            "con_id": 756733,
        },
        "native": {
            "min_size": details.min_size,
            "size_increment": details.size_increment,
            "suggested_size_increment": details.suggested_size_increment,
            "min_tick": details.min_tick,
        },
        "published": {
            "instrument_id": equity.id.to_string(),
            "venue": equity.id.venue.to_string(),
            "asset_class": format!("{:?}", equity.asset_class()),
            "size_precision": equity.size_precision(),
            "price_precision": equity.price_precision(),
            "price_increment": equity.price_increment().to_string(),
            "lot_size": equity.lot_size().map(|value| value.to_string()),
            "min_quantity": equity.min_quantity().map(|value| value.to_string()),
            "min_quantity_precision": equity.min_quantity().map(|value| value.precision),
            "raw_symbol": equity.raw_symbol.to_string(),
            "currency": equity.currency.to_string(),
        },
        "info": info,
    });

    let text = serde_json::to_string_pretty(&payload).expect("serialize fixture replay");
    std::fs::write(&out_path, text).expect("write fixture replay");
    eprintln!("wrote live SPY fixture replay to {}", out_path.display());
}
