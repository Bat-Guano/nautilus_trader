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

//! Instrument parsing utilities for converting IB ContractDetails to Nautilus instruments.

use std::str::FromStr;

use anyhow::Context;
use ibapi::contracts::SecurityType;
use nautilus_core::{DurationNanos, UnixNanos, time::get_atomic_clock_realtime};
use nautilus_model::{
    enums::AssetClass,
    identifiers::{InstrumentId, Symbol},
    instruments::{
        Cfd, Commodity, CryptoPerpetual, CurrencyPair, Equity, FuturesContract, FuturesSpread,
        IndexInstrument, InstrumentAny, OptionContract, OptionSpread,
    },
    types::{Currency, Price, Quantity},
};
use rust_decimal::Decimal;
use ustr::Ustr;

use crate::common::{
    contract_to_params,
    enums::{IbOptionRight, IbSecurityType},
};

const NINETY_DAYS: DurationNanos = DurationNanos::from_days(90);

/// The size precision an equity instrument carries, and therefore the only
/// precision at which a native minimum order size may be published for one.
///
/// `Equity::size_precision()` is `0` (whole shares).  Publishing a minimum at
/// any other precision would advertise a minimum the instrument itself cannot
/// represent.
const EQUITY_SIZE_PRECISION: u8 = 0;

/// The `info` provenance value recording that the minimum quantity came from
/// the native contract-details field.
const MIN_QUANTITY_SOURCE_NATIVE: &str = "contract_details.min_size";

/// The `info` provenance value recording that the native field was absent.
const MIN_QUANTITY_SOURCE_MISSING: &str = "contract_details.min_size.missing";

/// The `info` provenance value recording a native `min_size` that was not a
/// finite number (`NaN` or an infinity).
const MIN_QUANTITY_SOURCE_NON_FINITE: &str = "contract_details.min_size.non_finite";

/// The `info` provenance value recording a native `min_size` of zero or less.
const MIN_QUANTITY_SOURCE_NON_POSITIVE: &str = "contract_details.min_size.non_positive";

/// The `info` provenance value recording a native `min_size` with a fractional
/// part, which the whole-share equity model cannot represent.
const MIN_QUANTITY_SOURCE_FRACTIONAL: &str =
    "contract_details.min_size.fractional_not_representable";

/// The `info` provenance value recording a native `min_size` that is positive
/// and whole but still not representable as an equity [`Quantity`] (below the
/// smallest representable whole share, or outside the model's range).
const MIN_QUANTITY_SOURCE_UNREPRESENTABLE: &str =
    "contract_details.min_size.not_representable_at_equity_precision";

/// Native minimum order size from the IB contract-details response, resolved to
/// an equity minimum quantity **only when the native value is usable**.
///
/// C2.10-E2-R2-R1.  The earlier revision of this function manufactured a
/// one-share minimum whenever the native value was missing or unusable, and
/// pushed every positive value through `Quantity::new(native, 0)`.
/// Both behaviours were wrong:
///
/// * a manufactured minimum is not a fact from the venue, and nothing in the
///   adapter positively establishes that an arbitrary equity contract is
///   eligible for a one-share minimum; and
/// * `Quantity::new(value, 0)` silently ROUNDS to the requested precision
///   (`f64_to_fixed_u128` does `(value * 10^precision).round()`), so a
///   fractional native minimum would have been misrepresented as a whole
///   share - `0.5` becomes `1`, `1.5` becomes `2`.  `Quantity::new` also
///   PANICS outside `[QUANTITY_MIN, QUANTITY_MAX]`.
///
/// The rule is therefore: publish a minimum ONLY for a native `min_size` that
/// is finite, strictly positive, exactly whole, and representable by the model
/// at the instrument's own size precision.  Everything else publishes NO
/// minimum and records why in `info["minQuantitySource"]`, so the existing
/// instrument-legality gate refuses the request instead of a value being
/// invented or rounded.  Nothing is ever inferred from `lot_size`,
/// `size_increment` or `suggested_size_increment`, and no symbol-specific
/// logic exists here.
#[must_use]
fn equity_min_quantity(
    details: &ibapi::contracts::ContractDetails,
) -> (Option<Quantity>, &'static str) {
    let native = details.min_size;

    if native == 0.0 {
        // Covers both an unset protobuf field (the default `f64` is 0.0) and
        // an explicit zero, which are indistinguishable on the wire.
        return (None, MIN_QUANTITY_SOURCE_MISSING);
    }

    if !native.is_finite() {
        return (None, MIN_QUANTITY_SOURCE_NON_FINITE);
    }

    if native < 0.0 {
        return (None, MIN_QUANTITY_SOURCE_NON_POSITIVE);
    }

    if native.fract() != 0.0 {
        // Fractional-share trading is explicitly NOT claimed by this adapter,
        // and the equity model has no precision to carry a fraction.
        return (None, MIN_QUANTITY_SOURCE_FRACTIONAL);
    }

    // Let the model itself decide representability rather than re-deriving its
    // bounds here.  `new_checked` is the non-panicking door, and it also
    // catches a positive whole value that would round down to zero.
    match Quantity::new_checked(native, EQUITY_SIZE_PRECISION) {
        Ok(quantity) if !quantity.is_zero() => (Some(quantity), MIN_QUANTITY_SOURCE_NATIVE),
        _ => (None, MIN_QUANTITY_SOURCE_UNREPRESENTABLE),
    }
}

/// Convert tick size to precision value.
#[must_use]
pub fn tick_size_to_precision(tick_size: f64) -> u8 {
    if tick_size <= 0.0 {
        return 8; // Default precision for zero or negative tick sizes
    }

    // Count decimal places
    let s = format!("{:.10}", tick_size);
    let s = s.trim_end_matches('0');
    let parts: Vec<&str> = s.split('.').collect();

    if parts.len() == 2 {
        parts[1].len().min(8) as u8
    } else {
        0
    }
}

/// Convert timestamp string to UnixNanos.
///
/// Handles formats like "20230101" or "20230101 00:00:00 UTC".
///
/// # Errors
///
/// Returns an error if the timestamp cannot be parsed.
pub fn expiry_timestring_to_unix_nanos(
    expiry: &str,
    details: Option<&ibapi::contracts::ContractDetails>,
) -> anyhow::Result<UnixNanos> {
    if expiry.is_empty() {
        anyhow::bail!("Empty expiry string");
    }

    // Parse timestamp string - Most contract expirations are %Y%m%d format
    // Some exchanges have expirations in %Y%m%d %H:%M:%S %Z
    let dt = if expiry.len() == 8 {
        // Format: YYYYMMDD
        let year = &expiry[0..4];
        let month = &expiry[4..6];
        let day = &expiry[6..8];
        let date = time::Date::from_calendar_date(
            year.parse()?,
            time::Month::try_from(month.parse::<u8>()?)?,
            day.parse()?,
        )?;

        // If we have trading hours, try to extract the last trade time
        // Trading hours format: "20240411:0000-20240411:1800;..."
        let mut expiry_time = time::Time::MIDNIGHT;

        if let Some(details) = details {
            if !details.trading_hours.is_empty()
                && !details.trading_hours.contains(&"CLOSED".to_string())
            {
                // Find the session for this date
                let expiry_str: &str = expiry;
                for session in &details.trading_hours {
                    if session.as_str().starts_with(expiry_str) && session.as_str().contains('-') {
                        let parts: Vec<&str> = session.as_str().split('-').collect();
                        if let Some(end_part) = parts.get(1) {
                            let inner_parts: Vec<&str> = end_part.split(':').collect();
                            if let Some(time_part) = inner_parts.get(1) {
                                if time_part.len() >= 4 {
                                    let hour = time_part
                                        .get(0..2)
                                        .and_then(|s: &str| s.parse::<u8>().ok())
                                        .unwrap_or(0);
                                    let minute = time_part
                                        .get(2..4)
                                        .and_then(|s: &str| s.parse::<u8>().ok())
                                        .unwrap_or(0);
                                    expiry_time = time::Time::from_hms(hour, minute, 0)
                                        .unwrap_or(time::Time::MIDNIGHT);
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
        time::PrimitiveDateTime::new(date, expiry_time)
    } else {
        // Format: YYYYMMDD HH:MM:SS TZ
        let parts: Vec<&str> = expiry.split(' ').collect();
        if parts.len() >= 3 {
            let date_part = parts[0];
            let time_part = parts[1];
            let year = &date_part[0..4];
            let month = &date_part[4..6];
            let day = &date_part[6..8];

            let time_parts: Vec<&str> = time_part.split(':').collect();
            let hour = time_parts.first().unwrap_or(&"0").parse::<u8>()?;
            let minute = time_parts.get(1).unwrap_or(&"0").parse::<u8>()?;
            let second = time_parts.get(2).unwrap_or(&"0").parse::<u8>()?;

            let date = time::Date::from_calendar_date(
                year.parse()?,
                time::Month::try_from(month.parse::<u8>()?)?,
                day.parse()?,
            )?;
            let time_obj = time::Time::from_hms(hour, minute, second)?;
            time::PrimitiveDateTime::new(date, time_obj)
        } else {
            anyhow::bail!("Invalid expiry format: {}", expiry);
        }
    };

    // Treat the parsed expiry timestamp as UTC. NautilusTrader expects IB timestamps
    // to be configured and interpreted in UTC.
    let offset_dt = dt.assume_utc();
    let nanos = offset_dt.unix_timestamp_nanos();
    Ok(UnixNanos::new(nanos as u64))
}

/// Parse an IB ContractDetails to a Nautilus instrument.
///
/// # Errors
///
/// Returns an error if parsing fails.
pub fn parse_ib_contract_to_instrument(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> anyhow::Result<InstrumentAny> {
    let sec_type = &details.contract.security_type;

    match sec_type {
        SecurityType::Stock => Ok(parse_equity_contract(details, instrument_id)),
        SecurityType::ForexPair => Ok(parse_forex_contract(details, instrument_id)),
        SecurityType::Crypto => Ok(parse_crypto_contract(details, instrument_id)),
        SecurityType::Future | SecurityType::ContinuousFuture => {
            Ok(parse_futures_contract(details, instrument_id))
        }
        SecurityType::Option => parse_option_contract(details, instrument_id),
        SecurityType::FuturesOption => parse_option_contract(details, instrument_id), // FOP uses same parsing as OPT
        SecurityType::Index => Ok(parse_index_contract(details, instrument_id)),
        SecurityType::CFD => Ok(parse_cfd_contract(details, instrument_id)),
        SecurityType::Commodity => Ok(parse_commodity_contract(details, instrument_id)),
        SecurityType::Bond => Ok(parse_bond_contract(details, instrument_id)),
        _ => anyhow::bail!("Unsupported security type: {:?}", sec_type),
    }
}

fn ib_contract_info(details: &ibapi::contracts::ContractDetails) -> nautilus_core::Params {
    let mut info = nautilus_core::Params::new();
    let mut contract = serde_json::Map::new();

    let contract_params = contract_to_params(&details.contract);
    for (key, value) in &contract_params {
        contract.insert(key.clone(), value.clone());
    }

    info.insert("contract".to_string(), serde_json::Value::Object(contract));
    info.insert(
        "priceMagnifier".to_string(),
        serde_json::Value::from(details.price_magnifier),
    );
    info
}

fn ib_contract_info_for_contract(contract: &ibapi::contracts::Contract) -> nautilus_core::Params {
    let mut info = nautilus_core::Params::new();
    let mut contract_map = serde_json::Map::new();
    let contract_params = contract_to_params(contract);

    for (key, value) in &contract_params {
        contract_map.insert(key.clone(), value.clone());
    }

    info.insert(
        "contract".to_string(),
        serde_json::Value::Object(contract_map),
    );
    info
}

fn sec_type_to_asset_class(sec_type: &str) -> AssetClass {
    match IbSecurityType::from_str(sec_type).ok() {
        Some(IbSecurityType::Stock) => AssetClass::Equity,
        Some(IbSecurityType::Index) => AssetClass::Index,
        Some(IbSecurityType::ForexPair) => AssetClass::FX,
        Some(IbSecurityType::Bond) => AssetClass::Debt,
        Some(IbSecurityType::Commodity) => AssetClass::Commodity,
        Some(IbSecurityType::Future) => AssetClass::Index,
        _ => AssetClass::Equity,
    }
}

/// Parse equity contract (STK).
fn parse_equity_contract(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> InstrumentAny {
    let price_precision = tick_size_to_precision(details.min_tick);
    let timestamp = get_atomic_clock_realtime().get_time_ns();

    // C2.10-E2-R2 / R1: the equity minimum executable quantity is propagated
    // from the native IB contract-details response, and ONLY when that native
    // value is usable as a whole-share minimum.  The builder never set
    // `min_quantity` before this correction, so `Equity::min_quantity` stayed
    // `None`, the instrument-legality gate downstream could not establish the
    // legality of any quantity, and EVERY equity request was refused before
    // submission.  When the native metadata is absent or unusable the field is
    // deliberately left absent again, so that gate refuses instead of a value
    // being invented, rounded or inferred from another field.
    // `price_precision`, `price_increment`, `lot_size` (the round lot) and the
    // instrument identity are unchanged.
    let (min_quantity, min_quantity_source) = equity_min_quantity(details);

    let mut info = ib_contract_info(details);
    info.insert(
        "minSize".to_string(),
        serde_json::Value::from(details.min_size),
    );
    info.insert(
        "sizeIncrement".to_string(),
        serde_json::Value::from(details.size_increment),
    );
    info.insert(
        "suggestedSizeIncrement".to_string(),
        serde_json::Value::from(details.suggested_size_increment),
    );
    info.insert(
        "minQuantitySource".to_string(),
        serde_json::Value::from(min_quantity_source),
    );

    let instrument = Equity::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(details.contract.local_symbol.as_str()))
        .currency(Currency::from(details.contract.currency.to_string()))
        .price_precision(price_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        // `None` (absent or unusable native metadata) leaves `min_quantity`
        // UNSET, which is the deliberate fail-closed outcome: the downstream
        // instrument-legality gate then refuses any quantity for this
        // instrument instead of a minimum being manufactured for it.
        .maybe_min_quantity(min_quantity)
        // Standard lot size for stocks
        .lot_size(Quantity::new(100.0, 0))
        .info(info)
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()
        .unwrap();

    InstrumentAny::from(instrument)
}

/// Parse forex contract (CASH).
fn parse_forex_contract(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> InstrumentAny {
    let price_precision = tick_size_to_precision(details.min_tick);
    let size_precision = tick_size_to_precision(details.min_size);
    let timestamp = get_atomic_clock_realtime().get_time_ns();

    let instrument = CurrencyPair::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(details.contract.local_symbol.as_str()))
        .base_currency(Currency::from(details.contract.symbol.to_string()))
        .quote_currency(Currency::from(details.contract.currency.to_string()))
        .price_precision(price_precision)
        .size_precision(size_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        .size_increment(Quantity::new(details.size_increment, size_precision))
        .info(ib_contract_info(details))
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()
        .unwrap();

    InstrumentAny::from(instrument)
}

/// Parse crypto contract (CRYPTO).
fn parse_crypto_contract(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> InstrumentAny {
    let price_precision = tick_size_to_precision(details.min_tick);
    let size_precision = tick_size_to_precision(details.min_size);
    let timestamp = get_atomic_clock_realtime().get_time_ns();

    let instrument = CryptoPerpetual::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(details.contract.local_symbol.as_str()))
        .base_currency(Currency::from(details.contract.symbol.to_string()))
        .quote_currency(Currency::from(details.contract.currency.to_string()))
        .settlement_currency(Currency::from(details.contract.currency.to_string()))
        .is_inverse(true)
        .price_precision(price_precision)
        .size_precision(size_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        .size_increment(Quantity::new(details.size_increment, size_precision))
        .min_quantity(Quantity::new(details.min_size, size_precision))
        .info(ib_contract_info(details))
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()
        .unwrap();

    InstrumentAny::from(instrument)
}

fn parse_contract_multiplier(multiplier: &str, default: f64) -> Quantity {
    if multiplier.is_empty() {
        return Quantity::new(default, 0);
    }

    Quantity::from_str(multiplier).unwrap_or_else(|e| {
        tracing::warn!(
            "Failed to parse IB contract multiplier '{multiplier}', using default {default}: {e}"
        );
        Quantity::new(default, 0)
    })
}

/// Parse futures contract (FUT).
fn parse_futures_contract(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> InstrumentAny {
    let price_precision = tick_size_to_precision(details.min_tick);
    let timestamp = get_atomic_clock_realtime().get_time_ns();

    // Parse expiration
    let expiration_ns = if !details
        .contract
        .last_trade_date_or_contract_month
        .is_empty()
    {
        expiry_timestring_to_unix_nanos(
            &details.contract.last_trade_date_or_contract_month,
            Some(details),
        )
        .unwrap_or_else(|_| timestamp + NINETY_DAYS)
    // Default to +90 days on error
    } else {
        timestamp + NINETY_DAYS // Default to +90 days if empty
    };

    let activation_ns = expiration_ns
        .checked_sub(NINETY_DAYS)
        .unwrap_or(UnixNanos::from(0)); // -90 days or 0 if underflow

    let multiplier = parse_contract_multiplier(&details.contract.multiplier, 1.0);

    let raw_symbol = if matches!(
        details.contract.security_type,
        SecurityType::ContinuousFuture
    ) && !details.contract.symbol.as_str().is_empty()
    {
        details.contract.symbol.as_str()
    } else {
        details.contract.local_symbol.as_str()
    };

    let instrument = FuturesContract::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(raw_symbol))
        .asset_class(sec_type_to_asset_class(
            details.under_security_type.as_str(),
        ))
        .underlying(Ustr::from(details.under_symbol.as_str()))
        .activation_ns(activation_ns)
        .expiration_ns(expiration_ns)
        .currency(Currency::from(details.contract.currency.to_string()))
        .price_precision(price_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        .multiplier(multiplier)
        .lot_size(Quantity::new(1.0, 0))
        .info(ib_contract_info(details))
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()
        .unwrap();

    InstrumentAny::from(instrument)
}

/// Parse option contract (OPT).
fn parse_option_contract(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> anyhow::Result<InstrumentAny> {
    let price_precision = tick_size_to_precision(details.min_tick);
    let timestamp = get_atomic_clock_realtime().get_time_ns();

    // Parse expiration
    let expiration_ns = if !details
        .contract
        .last_trade_date_or_contract_month
        .is_empty()
    {
        expiry_timestring_to_unix_nanos(
            &details.contract.last_trade_date_or_contract_month,
            Some(details),
        )
        .unwrap_or_else(|_| timestamp + NINETY_DAYS)
    // Default to +90 days on error
    } else {
        timestamp + NINETY_DAYS // Default to +90 days if empty
    };

    let activation_ns = expiration_ns
        .checked_sub(NINETY_DAYS)
        .unwrap_or(UnixNanos::from(0)); // -90 days or 0 if underflow

    // Parse option kind (CALL or PUT)
    let option_kind = details
        .contract
        .right
        .map(|right| IbOptionRight::from_str(right.as_str()))
        .transpose()?
        .context("Option contract missing right")?
        .option_kind();

    let multiplier = parse_contract_multiplier(&details.contract.multiplier, 100.0);
    let asset_class = sec_type_to_asset_class(details.under_security_type.as_str());
    let underlying =
        if details.under_security_type == "IND" && !details.under_symbol.starts_with('^') {
            format!("^{}", details.under_symbol)
        } else {
            details.under_symbol.clone()
        };

    let instrument = OptionContract::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(details.contract.local_symbol.as_str()))
        .asset_class(asset_class)
        .underlying(Ustr::from(underlying.as_str()))
        .option_kind(option_kind)
        .strike_price(Price::new(details.contract.strike, price_precision))
        .currency(Currency::from(details.contract.currency.to_string()))
        .activation_ns(activation_ns)
        .expiration_ns(expiration_ns)
        .price_precision(price_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        .multiplier(multiplier)
        .lot_size(multiplier)
        .info(ib_contract_info(details))
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()
        .unwrap();

    Ok(InstrumentAny::from(instrument))
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use ibapi::contracts::{
        Contract, ContractDetails, Currency, Exchange, OptionRight, SecurityType, Symbol,
    };
    use nautilus_model::{
        enums::AssetClass,
        identifiers::{InstrumentId, Symbol as NautilusSymbol, Venue},
        instruments::{Equity, Instrument, InstrumentAny},
        types::{Price, Quantity},
    };
    use rstest::rstest;

    // C2.10-E2-R2-R1: the provenance labels the parser publishes, so the tests
    // assert the EXACT reason an unusable native minimum was not published.
    use super::{
        MIN_QUANTITY_SOURCE_FRACTIONAL, MIN_QUANTITY_SOURCE_MISSING, MIN_QUANTITY_SOURCE_NATIVE,
        MIN_QUANTITY_SOURCE_NON_FINITE, MIN_QUANTITY_SOURCE_NON_POSITIVE,
        MIN_QUANTITY_SOURCE_UNREPRESENTABLE,
    };
    use ustr::Ustr;

    use super::{
        parse_contract_multiplier, parse_ib_contract_to_instrument,
        parse_option_spread_instrument_id,
    };

    #[rstest]
    fn test_parse_option_contract_prefixes_index_underlying() {
        let details = ContractDetails {
            contract: Contract {
                symbol: Symbol::from("SPXW"),
                security_type: SecurityType::Option,
                exchange: Exchange::from("SMART"),
                currency: Currency::from("USD"),
                local_symbol: "SPXW  260313P06630000".to_string(),
                last_trade_date_or_contract_month: "20260313".to_string(),
                right: Some(OptionRight::Put),
                strike: 6630.0,
                multiplier: "100".to_string(),
                ..Default::default()
            },
            min_tick: 0.05,
            under_symbol: "SPX".to_string(),
            under_security_type: "IND".to_string(),
            ..Default::default()
        };
        let instrument_id = InstrumentId::new(
            NautilusSymbol::from("SPXW  260313P06630000"),
            Venue::from("SMART"),
        );

        let instrument = parse_ib_contract_to_instrument(&details, instrument_id).unwrap();

        let InstrumentAny::OptionContract(option) = instrument else {
            panic!("expected option contract");
        };

        assert_eq!(option.asset_class(), AssetClass::Index);
        assert_eq!(option.underlying(), Some(Ustr::from("^SPX")));
    }

    #[rstest]
    fn test_parse_contract_preserves_price_magnifier_in_info() {
        let details = ContractDetails {
            contract: Contract {
                symbol: Symbol::from("AAPL"),
                security_type: SecurityType::Stock,
                exchange: Exchange::from("SMART"),
                primary_exchange: Exchange::from("NASDAQ"),
                currency: Currency::from("USD"),
                local_symbol: String::from("AAPL"),
                ..Default::default()
            },
            min_tick: 0.01,
            price_magnifier: 100,
            ..Default::default()
        };
        let instrument_id = InstrumentId::new(NautilusSymbol::from("AAPL"), Venue::from("XNAS"));

        let instrument = parse_ib_contract_to_instrument(&details, instrument_id).unwrap();
        let InstrumentAny::Equity(equity) = instrument else {
            panic!("expected equity");
        };

        assert_eq!(
            equity.info.unwrap().get("priceMagnifier"),
            Some(&serde_json::Value::from(100))
        );
    }

    // ------------------------------------------------------------------- //
    // C2.10-E2-R2: native equity minimum-quantity propagation              //
    // ------------------------------------------------------------------- //
    //
    // The IB contract-details response carries `min_size` ("Order's minimal
    // size"), `size_increment` and `suggested_size_increment`.  The accepted
    // `parse_equity_contract` dropped all three, so every resolved equity
    // published `min_quantity = None` and no equity request could establish
    // quantity legality.  These tests pin the propagation.

    /// A US stock/ETF `ContractDetails` as the `SecurityType::Stock` path
    /// receives it from the IB protobuf contract-details response.
    fn stock_details(
        symbol: &str,
        local_symbol: &str,
        primary_exchange: &str,
        min_size: f64,
    ) -> ContractDetails {
        ContractDetails {
            contract: Contract {
                symbol: Symbol::from(symbol),
                security_type: SecurityType::Stock,
                exchange: Exchange::from("SMART"),
                primary_exchange: Exchange::from(primary_exchange),
                currency: Currency::from("USD"),
                local_symbol: local_symbol.to_string(),
                ..Default::default()
            },
            min_tick: 0.01,
            min_size,
            size_increment: 1.0,
            suggested_size_increment: 100.0,
            ..Default::default()
        }
    }

    fn stock_details_with_increments(
        symbol: &str,
        primary_exchange: &str,
        min_size: f64,
        size_increment: f64,
        suggested_size_increment: f64,
    ) -> ContractDetails {
        ContractDetails {
            size_increment,
            suggested_size_increment,
            ..stock_details(symbol, symbol, primary_exchange, min_size)
        }
    }

    fn stock_equity(symbol: &str, primary_exchange: &str, min_size: f64) -> (Equity, InstrumentId) {
        let details = stock_details(symbol, symbol, primary_exchange, min_size);
        let instrument_id =
            InstrumentId::new(NautilusSymbol::from(symbol), Venue::from(primary_exchange));
        let instrument = parse_ib_contract_to_instrument(&details, instrument_id).unwrap();
        let InstrumentAny::Equity(equity) = instrument else {
            panic!("expected equity for {symbol}.{primary_exchange}");
        };
        (equity, instrument_id)
    }

    #[rstest]
    #[case("SPY", "ARCA")]
    #[case("AAPL", "NASDAQ")]
    #[case("MSFT", "NASDAQ")]
    fn test_parse_equity_contract_publishes_native_min_quantity(
        #[case] symbol: &str,
        #[case] primary_exchange: &str,
    ) {
        let (equity, instrument_id) = stock_equity(symbol, primary_exchange, 1.0);

        assert_eq!(equity.asset_class(), AssetClass::Equity);
        assert_eq!(equity.id, instrument_id);
        // The native minimum size is ONE SHARE, taken from `min_size`, not
        // from the round-lot `lot_size` of 100 and not from
        // `suggested_size_increment` of 100.
        assert_eq!(
            equity.min_quantity(),
            Some(Quantity::new(1.0, 0)),
            "the resolved equity must publish the native contract-details \
             minimum order size"
        );
    }

    #[rstest]
    fn test_parse_equity_contract_min_quantity_precision_matches_instrument() {
        let (equity, _) = stock_equity("SPY", "ARCA", 1.0);
        let min_quantity = equity.min_quantity().expect("min_quantity published");

        assert_eq!(min_quantity.precision, equity.size_precision());
        assert_eq!(equity.size_precision(), 0);
    }

    #[rstest]
    fn test_parse_equity_contract_preserves_price_increment_and_lot_size() {
        let (equity, _) = stock_equity("SPY", "ARCA", 1.0);

        // Unchanged by the correction.
        assert_eq!(equity.price_precision(), 2);
        assert_eq!(equity.price_increment(), Price::new(0.01, 2));
        assert_eq!(equity.lot_size(), Some(Quantity::new(100.0, 0)));
    }

    #[rstest]
    fn test_parse_equity_contract_does_not_substitute_lot_size_for_min_quantity() {
        let (equity, _) = stock_equity("SPY", "ARCA", 1.0);

        // A round lot (100) and the minimum executable size (1) are
        // different concepts: the adapter must not silently substitute one
        // for the other.
        assert_ne!(equity.min_quantity(), equity.lot_size());
        assert_eq!(equity.min_quantity(), Some(Quantity::new(1.0, 0)));
        assert_eq!(equity.lot_size(), Some(Quantity::new(100.0, 0)));
    }

    #[rstest]
    fn test_parse_equity_contract_records_native_min_size_provenance() {
        let (equity, _) = stock_equity("SPY", "ARCA", 1.0);
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from("contract_details.min_size")),
            "the minimum quantity must be traceable to the native field it \
             came from"
        );
        assert_eq!(info.get("minSize"), Some(&serde_json::Value::from(1.0)),);
        assert_eq!(
            info.get("sizeIncrement"),
            Some(&serde_json::Value::from(1.0)),
        );
        assert_eq!(
            info.get("suggestedSizeIncrement"),
            Some(&serde_json::Value::from(100.0)),
        );
    }

    /// C2.10-E2-R2-R1 requirement 1: `min_size = 1.0` is the native minimum.
    #[rstest]
    #[case("SPY", "ARCA")]
    #[case("AAPL", "NASDAQ")]
    #[case("MSFT", "NASDAQ")]
    fn test_parse_equity_contract_native_min_size_one_is_published(
        #[case] symbol: &str,
        #[case] primary_exchange: &str,
    ) {
        let (equity, _) = stock_equity(symbol, primary_exchange, 1.0);
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(equity.min_quantity(), Some(Quantity::new(1.0, 0)));
        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(MIN_QUANTITY_SOURCE_NATIVE)),
        );
    }

    /// C2.10-E2-R2-R1 requirement 2: another whole-share value stays EXACT.
    #[rstest]
    #[case(5.0, 5.0)]
    #[case(100.0, 100.0)]
    #[case(1.0, 1.0)]
    fn test_parse_equity_contract_native_min_size_whole_value_is_exact(
        #[case] min_size: f64,
        #[case] expected: f64,
    ) {
        let (equity, _) = stock_equity("SPY", "ARCA", min_size);
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(
            equity.min_quantity(),
            Some(Quantity::new(expected, 0)),
            "a native whole-share minimum must be published EXACTLY, never \
             rounded and never substituted"
        );
        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(MIN_QUANTITY_SOURCE_NATIVE)),
        );
    }

    /// Requirements 3, 4, 6, 7: non-positive and non-finite native values must
    /// NOT become a manufactured one-share minimum.  Each must leave the
    /// minimum UNSET and record explicit invalid provenance.
    #[rstest]
    #[case(0.0, MIN_QUANTITY_SOURCE_MISSING)]
    #[case(-1.0, MIN_QUANTITY_SOURCE_NON_POSITIVE)]
    #[case(-0.5, MIN_QUANTITY_SOURCE_NON_POSITIVE)]
    #[case(f64::NAN, MIN_QUANTITY_SOURCE_NON_FINITE)]
    #[case(f64::INFINITY, MIN_QUANTITY_SOURCE_NON_FINITE)]
    #[case(f64::NEG_INFINITY, MIN_QUANTITY_SOURCE_NON_FINITE)]
    fn test_parse_equity_contract_invalid_native_min_size_leaves_minimum_unset(
        #[case] min_size: f64,
        #[case] expected_source: &str,
    ) {
        let (equity, _) = stock_equity("SPY", "ARCA", min_size);
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(
            equity.min_quantity(),
            None,
            "native min_size {min_size} must NOT produce a usable equity \
             minimum; manufacturing one would state a venue fact that was \
             never observed"
        );
        assert_ne!(
            equity.min_quantity(),
            Some(Quantity::new(1.0, 0)),
            "the removed one-share fallback must not survive in any form"
        );
        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(expected_source)),
            "unusable native metadata must record WHY no minimum was published"
        );
        // The raw native value is still recorded for diagnostics.  Note that
        // `serde_json` maps NaN and the infinities to `Value::Null`, so the
        // expectation is the JSON spelling, not the float.
        if min_size.is_finite() {
            assert_eq!(
                info.get("minSize").and_then(serde_json::Value::as_f64),
                Some(min_size),
            );
        } else {
            assert_eq!(info.get("minSize"), Some(&serde_json::Value::Null));
        }
    }

    /// Requirements 8 and 9: a FRACTIONAL native minimum must never be rounded
    /// into a whole share.  `Quantity::new(0.5, 0)` would silently yield 1 and
    /// `Quantity::new(1.5, 0)` would silently yield 2, because
    /// `f64_to_fixed_u128` rounds to the requested precision.  This is the
    /// defect the R1 correction removes.
    #[rstest]
    #[case(0.5)]
    #[case(1.5)]
    #[case(2.25)]
    #[case(0.999)]
    #[case(f64::MIN_POSITIVE)]
    fn test_parse_equity_contract_fractional_native_min_size_is_never_rounded(
        #[case] min_size: f64,
    ) {
        let (equity, _) = stock_equity("SPY", "ARCA", min_size);
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(
            equity.min_quantity(),
            None,
            "fractional native min_size {min_size} must not be published for a \
             whole-share instrument"
        );
        // And specifically not the rounded whole share it would have become.
        assert_ne!(equity.min_quantity(), Some(Quantity::new(1.0, 0)));
        assert_ne!(equity.min_quantity(), Some(Quantity::new(2.0, 0)));
        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(MIN_QUANTITY_SOURCE_FRACTIONAL)),
        );
        // No fractional-share support is claimed anywhere.
        assert_eq!(equity.size_precision(), 0);
    }

    /// A positive whole value that the model cannot represent must fail closed
    /// rather than PANIC `Quantity::new` or be silently clamped: `1.0e30` and
    /// `f64::MAX` both exceed `QUANTITY_MAX`, and `Quantity::new` PANICS on
    /// out-of-range input, so the non-panicking `new_checked` door is used.
    #[rstest]
    #[case(1.0e30)]
    #[case(f64::MAX)]
    fn test_parse_equity_contract_out_of_range_native_min_size_fails_closed(#[case] min_size: f64) {
        let (equity, _) = stock_equity("SPY", "ARCA", min_size);
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(equity.min_quantity(), None);
        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(
                MIN_QUANTITY_SOURCE_UNREPRESENTABLE
            )),
        );
    }

    /// Requirement 10: `lot_size` (the round lot, 100) is a DIFFERENT concept
    /// and stays exactly 100 for every native minimum, valid or not.
    #[rstest]
    #[case(1.0)]
    #[case(5.0)]
    #[case(0.0)]
    #[case(-1.0)]
    #[case(f64::NAN)]
    #[case(f64::INFINITY)]
    #[case(0.5)]
    fn test_parse_equity_contract_lot_size_remains_distinct_and_unchanged(#[case] min_size: f64) {
        let (equity, _) = stock_equity("SPY", "ARCA", min_size);

        assert_eq!(equity.lot_size(), Some(Quantity::new(100.0, 0)));
        assert_ne!(
            equity.min_quantity(),
            equity.lot_size(),
            "the round lot must never be substituted for the minimum"
        );
    }

    /// Requirement 11: neither `size_increment` nor
    /// `suggested_size_increment` may be substituted for the minimum.
    #[rstest]
    #[case(1.0, 5.0, 250.0)]
    #[case(5.0, 25.0, 500.0)]
    fn test_parse_equity_contract_size_increments_are_not_substituted(
        #[case] min_size: f64,
        #[case] size_increment: f64,
        #[case] suggested_size_increment: f64,
    ) {
        let details = stock_details_with_increments(
            "SPY",
            "ARCA",
            min_size,
            size_increment,
            suggested_size_increment,
        );
        let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));
        let InstrumentAny::Equity(equity) =
            parse_ib_contract_to_instrument(&details, instrument_id).unwrap()
        else {
            panic!("expected equity");
        };
        let info = equity.info.clone().expect("instrument info");

        // The published minimum is the native `min_size`, not either increment.
        assert_eq!(equity.min_quantity(), Some(Quantity::new(min_size, 0)));
        assert_ne!(
            equity.min_quantity(),
            Some(Quantity::new(size_increment, 0))
        );
        assert_ne!(
            equity.min_quantity(),
            Some(Quantity::new(suggested_size_increment, 0))
        );
        // And both increments are still recorded verbatim for auditability.
        assert_eq!(
            info.get("sizeIncrement"),
            Some(&serde_json::Value::from(size_increment)),
        );
        assert_eq!(
            info.get("suggestedSizeIncrement"),
            Some(&serde_json::Value::from(suggested_size_increment)),
        );
    }

    /// Requirement 11 (negative direction): with an UNUSABLE native minimum,
    /// usable increments must not rescue it into a minimum.
    #[rstest]
    fn test_parse_equity_contract_increments_do_not_rescue_unusable_minimum() {
        let details = stock_details_with_increments("SPY", "ARCA", 0.0, 1.0, 100.0);
        let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));
        let InstrumentAny::Equity(equity) =
            parse_ib_contract_to_instrument(&details, instrument_id).unwrap()
        else {
            panic!("expected equity");
        };

        assert_eq!(
            equity.min_quantity(),
            None,
            "size_increment=1 / suggested_size_increment=100 must NOT be \
             promoted into a minimum quantity"
        );
    }

    #[rstest]
    #[case("100", 100.0)]
    #[case("", 1.0)]
    #[case("not-a-number", 1.0)]
    fn test_parse_contract_multiplier_uses_quantity_parser(
        #[case] multiplier: &str,
        #[case] expected: f64,
    ) {
        assert_eq!(
            parse_contract_multiplier(multiplier, 1.0),
            Quantity::new(expected, 0)
        );
    }

    #[rstest]
    fn test_parse_continuous_future_contract_uses_symbol_as_raw_symbol() {
        let details = ContractDetails {
            contract: Contract {
                symbol: Symbol::from("ES"),
                security_type: SecurityType::ContinuousFuture,
                exchange: Exchange::from("CME"),
                currency: Currency::from("USD"),
                local_symbol: String::new(),
                multiplier: "50".to_string(),
                ..Default::default()
            },
            min_tick: 0.25,
            under_symbol: "ES".to_string(),
            under_security_type: "IND".to_string(),
            ..Default::default()
        };
        let instrument_id = InstrumentId::new(NautilusSymbol::from("ES"), Venue::from("CME"));

        let instrument = parse_ib_contract_to_instrument(&details, instrument_id).unwrap();

        let InstrumentAny::FuturesContract(future) = instrument else {
            panic!("expected futures contract");
        };

        assert_eq!(future.raw_symbol().as_str(), "ES");
    }

    #[rstest]
    fn test_parse_option_spread_uses_minimum_leg_tick() {
        let leg1 = ContractDetails {
            contract: Contract {
                symbol: Symbol::from("SPY"),
                security_type: SecurityType::Option,
                exchange: Exchange::from("SMART"),
                currency: Currency::from("USD"),
                local_symbol: "SPY   260120C00400000".to_string(),
                multiplier: "100".to_string(),
                ..Default::default()
            },
            min_tick: 0.05,
            under_symbol: "SPY".to_string(),
            ..Default::default()
        };
        let leg2 = ContractDetails {
            contract: Contract {
                symbol: Symbol::from("SPY"),
                security_type: SecurityType::Option,
                exchange: Exchange::from("SMART"),
                currency: Currency::from("USD"),
                local_symbol: "SPY   260120C00410000".to_string(),
                multiplier: "100".to_string(),
                ..Default::default()
            },
            min_tick: 0.01,
            under_symbol: "SPY".to_string(),
            ..Default::default()
        };
        let instrument_id =
            InstrumentId::from("(1)SPY   260120C00400000_((-1))SPY   260120C00410000.SMART");

        let spread = parse_option_spread_instrument_id(
            instrument_id,
            &[(&leg1, 1), (&leg2, -1)],
            None,
            None,
        )
        .unwrap();

        assert_eq!(spread.price_precision(), 2);
        assert_eq!(spread.price_increment(), Price::from("0.01"));
    }
}

/// Parse index contract (IND).
///
/// Note: Indices are typically not directly tradable. This creates a CurrencyPair
/// representation as a placeholder until IndexInstrument type is available.
fn parse_index_contract(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> InstrumentAny {
    let price_precision = tick_size_to_precision(details.min_tick);
    let size_precision = tick_size_to_precision(details.min_size);
    let timestamp = get_atomic_clock_realtime().get_time_ns();

    let instrument = IndexInstrument::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(details.contract.local_symbol.as_str()))
        .currency(Currency::from(details.contract.currency.to_string()))
        .price_precision(price_precision)
        .size_precision(size_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        .size_increment(Quantity::new(details.size_increment, size_precision))
        .info(ib_contract_info(details))
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()
        .unwrap();

    InstrumentAny::from(instrument)
}

/// Parse a spread instrument ID into an OptionSpread instrument.
///
/// This implements the same logic as Python's `parse_spread_instrument_id`.
/// Uses contract details from the first leg to determine spread properties.
///
/// # Errors
///
/// Returns an error if parsing fails.
pub fn parse_spread_instrument_id(
    instrument_id: InstrumentId,
    leg_contract_details: &[(&ibapi::contracts::ContractDetails, i32)],
    timestamp_ns: Option<UnixNanos>,
) -> anyhow::Result<OptionSpread> {
    if leg_contract_details.is_empty() {
        anyhow::bail!("leg_contract_details must be provided");
    }

    // Use contract details from first leg
    let (first_details, _) = leg_contract_details[0];
    let first_contract = &first_details.contract;

    // Extract properties from the first leg contract details
    let currency = Currency::from(first_contract.currency.to_string());
    let underlying = if !first_details.under_symbol.is_empty() {
        Ustr::from(first_details.under_symbol.as_str())
    } else {
        Ustr::from(first_contract.symbol.as_str())
    };

    // Parse multiplier
    let multiplier_str = first_contract.multiplier.to_string();
    let multiplier =
        Quantity::from_str(&multiplier_str).unwrap_or_else(|_| Quantity::new(100.0, 0)); // Default to 100 for options

    // Determine asset class based on security type
    let asset_class = match first_contract.security_type {
        ibapi::contracts::SecurityType::FuturesOption => AssetClass::Index, // Futures options
        _ => AssetClass::Equity,                                            // Equity options
    };

    // Calculate price precision and increment from the finest leg tick.
    let min_tick = leg_contract_details
        .iter()
        .map(|(details, _)| details.min_tick)
        .fold(first_details.min_tick, f64::min);
    let price_precision = tick_size_to_precision(min_tick);
    let price_increment = Price::new(min_tick, price_precision);

    // Use provided timestamp or current time
    let timestamp = timestamp_ns.unwrap_or_else(|| get_atomic_clock_realtime().get_time_ns());

    // For options spreads, lot size equals multiplier (same as individual option contracts)
    let lot_size = multiplier;

    // Create the spread instrument
    let spread = OptionSpread::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(instrument_id.symbol.as_str()))
        .asset_class(asset_class)
        .underlying(underlying)
        .strategy_type(Ustr::from("SPREAD"))
        // activation_ns (spreads don't have single activation dates)
        .activation_ns(UnixNanos::new(0))
        // expiration_ns (spreads don't have single expiration dates)
        .expiration_ns(UnixNanos::new(0))
        .currency(currency)
        .price_precision(price_precision)
        .price_increment(price_increment)
        .multiplier(multiplier)
        .lot_size(lot_size)
        .margin_init(Decimal::ZERO)
        .margin_maint(Decimal::ZERO)
        .maker_fee(Decimal::ZERO)
        .taker_fee(Decimal::ZERO)
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()?;

    Ok(spread)
}

pub fn parse_option_spread_instrument_id(
    instrument_id: InstrumentId,
    leg_contract_details: &[(&ibapi::contracts::ContractDetails, i32)],
    bag_contract: Option<&ibapi::contracts::Contract>,
    timestamp_ns: Option<UnixNanos>,
) -> anyhow::Result<OptionSpread> {
    let mut spread = parse_spread_instrument_id(instrument_id, leg_contract_details, timestamp_ns)?;
    spread.info = bag_contract.map(ib_contract_info_for_contract);
    Ok(spread)
}

pub fn parse_futures_spread_instrument_id(
    instrument_id: InstrumentId,
    leg_contract_details: &[(&ibapi::contracts::ContractDetails, i32)],
    bag_contract: Option<&ibapi::contracts::Contract>,
    timestamp_ns: Option<UnixNanos>,
) -> anyhow::Result<FuturesSpread> {
    if leg_contract_details.is_empty() {
        anyhow::bail!("leg_contract_details must be provided");
    }

    let (first_details, _) = leg_contract_details[0];
    let first_contract = &first_details.contract;
    let currency = Currency::from(first_contract.currency.to_string());
    let underlying = if !first_details.under_symbol.is_empty() {
        Ustr::from(first_details.under_symbol.as_str())
    } else {
        Ustr::from(first_contract.symbol.as_str())
    };
    let multiplier = Quantity::from_str(&first_contract.multiplier.to_string())
        .unwrap_or_else(|_| Quantity::new(1.0, 0));
    let min_tick = leg_contract_details
        .iter()
        .map(|(details, _)| details.min_tick)
        .fold(first_details.min_tick, f64::min);
    let price_precision = tick_size_to_precision(min_tick);
    let price_increment = Price::new(min_tick, price_precision);
    let timestamp = timestamp_ns.unwrap_or_else(|| get_atomic_clock_realtime().get_time_ns());

    Ok(FuturesSpread::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(instrument_id.symbol.as_str()))
        .asset_class(AssetClass::Index)
        .underlying(underlying)
        .strategy_type(Ustr::from("SPREAD"))
        .activation_ns(UnixNanos::new(0))
        .expiration_ns(UnixNanos::new(0))
        .currency(currency)
        .price_precision(price_precision)
        .price_increment(price_increment)
        .multiplier(multiplier)
        .lot_size(Quantity::new(1.0, 0))
        .margin_init(Decimal::ZERO)
        .margin_maint(Decimal::ZERO)
        .maker_fee(Decimal::ZERO)
        .taker_fee(Decimal::ZERO)
        .maybe_info(bag_contract.map(ib_contract_info_for_contract))
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()?)
}

pub fn parse_spread_instrument_any(
    instrument_id: InstrumentId,
    leg_contract_details: &[(&ibapi::contracts::ContractDetails, i32)],
    bag_contract: Option<&ibapi::contracts::Contract>,
    timestamp_ns: Option<UnixNanos>,
) -> anyhow::Result<InstrumentAny> {
    let has_future = leg_contract_details.iter().any(|(details, _)| {
        matches!(
            details.contract.security_type,
            SecurityType::Future | SecurityType::ContinuousFuture
        )
    });

    if has_future {
        Ok(InstrumentAny::from(parse_futures_spread_instrument_id(
            instrument_id,
            leg_contract_details,
            bag_contract,
            timestamp_ns,
        )?))
    } else {
        Ok(InstrumentAny::from(parse_option_spread_instrument_id(
            instrument_id,
            leg_contract_details,
            bag_contract,
            timestamp_ns,
        )?))
    }
}

/// Parse CFD contract (CFD).
fn parse_cfd_contract(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> InstrumentAny {
    let price_precision = tick_size_to_precision(details.min_tick);
    let size_precision = tick_size_to_precision(details.min_size);
    let timestamp = get_atomic_clock_realtime().get_time_ns();

    let base_currency = details
        .contract
        .local_symbol
        .contains('.')
        .then(|| Currency::from(details.contract.symbol.to_string()));

    let instrument = Cfd::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(details.contract.local_symbol.as_str()))
        .asset_class(sec_type_to_asset_class(
            details.under_security_type.as_str(),
        ))
        .maybe_base_currency(base_currency)
        .quote_currency(Currency::from(details.contract.currency.to_string()))
        .price_precision(price_precision)
        .size_precision(size_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        .size_increment(Quantity::new(details.size_increment, size_precision))
        .info(ib_contract_info(details))
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()
        .unwrap();

    InstrumentAny::from(instrument)
}

/// Parse commodity contract (CMDTY).
fn parse_commodity_contract(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> InstrumentAny {
    let price_precision = tick_size_to_precision(details.min_tick);
    let size_precision = tick_size_to_precision(details.min_size);
    let timestamp = get_atomic_clock_realtime().get_time_ns();

    let instrument = Commodity::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(details.contract.local_symbol.as_str()))
        .asset_class(AssetClass::Commodity)
        .quote_currency(Currency::from(details.contract.currency.to_string()))
        .price_precision(price_precision)
        .size_precision(size_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        .size_increment(Quantity::new(details.size_increment, size_precision))
        .info(ib_contract_info(details))
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()
        .unwrap();

    InstrumentAny::from(instrument)
}

/// Parse bond contract (BOND).
fn parse_bond_contract(
    details: &ibapi::contracts::ContractDetails,
    instrument_id: InstrumentId,
) -> InstrumentAny {
    // Use Equity as a placeholder until Bond type is available in Rust model
    // Note: This is a limitation of the current Nautilus Rust model, not the IB adapter
    let price_precision = tick_size_to_precision(details.min_tick);
    let timestamp = get_atomic_clock_realtime().get_time_ns();

    // ISIN could be extracted from `security_id` if needed
    let instrument = Equity::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(details.contract.local_symbol.as_str()))
        .currency(Currency::from(details.contract.currency.to_string()))
        .price_precision(price_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        // Standard lot size for bonds
        .lot_size(Quantity::new(1.0, 0))
        .info(ib_contract_info(details))
        .ts_event(timestamp)
        .ts_init(timestamp)
        .build()
        .unwrap();

    InstrumentAny::from(instrument)
}
