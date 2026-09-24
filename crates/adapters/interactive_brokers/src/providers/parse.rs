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

/// The `info` provenance value recording a native `min_size` that could not be
/// turned into a usable whole-share minimum at all: the exact decimal
/// restatement overflowed, the common-scale integer arithmetic overflowed, the
/// value falls outside the model's range, or it lies beyond the range in which
/// an `f64` reproduces every integer exactly.
const MIN_QUANTITY_SOURCE_UNREPRESENTABLE: &str =
    "contract_details.min_size.not_representable_at_equity_precision";

/// The `info` provenance value recording that the native `min_size` was
/// NORMALIZED onto the system's whole-share increment: the smallest whole-share
/// quantity at or above the native minimum was published, so the published
/// value is strictly greater than the native one and the raw value is kept
/// alongside it.
///
/// C2.10-E2-R3-Q2.  This is a NARROWING of the venue's constraint, never a
/// relaxation: the published minimum is always `>=` the native minimum, and the
/// native minimum is always a legal quantity of the system increment.
const MIN_QUANTITY_SOURCE_NORMALIZED: &str =
    "contract_details.min_size.ceiling_to_system_size_increment";

/// The `info` provenance value recording that the native `size_increment` was
/// absent (or an unset field, indistinguishable from an explicit zero).
///
/// A minimum cannot be normalized without knowing the lattice the venue trades
/// on, so an absent increment fails closed even when `min_size` is usable.
const MIN_QUANTITY_SOURCE_INCREMENT_MISSING: &str = "contract_details.size_increment.missing";

/// The `info` provenance value recording a native `size_increment` that was not
/// a finite number.
const MIN_QUANTITY_SOURCE_INCREMENT_NON_FINITE: &str = "contract_details.size_increment.non_finite";

/// The `info` provenance value recording a native `size_increment` of zero or
/// less.
const MIN_QUANTITY_SOURCE_INCREMENT_NON_POSITIVE: &str =
    "contract_details.size_increment.non_positive";

/// The `info` provenance value recording that the system's whole-share
/// increment does NOT lie on the venue's `size_increment` lattice, so the
/// system could not express its own one-share step as a whole number of venue
/// increments.  The alignment required for a safe normalization cannot be
/// proven, so nothing is published.
const MIN_QUANTITY_SOURCE_INCREMENT_INCOMPATIBLE: &str =
    "contract_details.size_increment.system_increment_not_on_lattice";

/// The `info` provenance value recording that the native `min_size` is not
/// itself a whole number of venue increments, i.e. the venue's own two
/// constraint fields are mutually inconsistent.  Nothing is inferred from
/// inconsistent metadata.
const MIN_QUANTITY_SOURCE_MIN_OFF_LATTICE: &str =
    "contract_details.min_size.not_on_size_increment_lattice";

/// The number of shares the system's whole-share equity model steps by.
///
/// `Equity::size_precision()` is `0`, so the smallest quantity the model can
/// express is exactly one share.  This is a property of the SYSTEM, not a fact
/// read from the venue, and it is the only system increment this adapter
/// normalizes onto.
const EQUITY_SYSTEM_SIZE_INCREMENT_SHARES: i128 = 1;

/// The `info` key recording the exact raw venue minimum order size.
const INFO_KEY_RAW_MIN_SIZE: &str = "minQuantityRawMinSize";

/// The `info` key recording the exact raw venue size increment.
const INFO_KEY_RAW_SIZE_INCREMENT: &str = "minQuantityRawSizeIncrement";

/// The `info` key recording the effective system minimum order quantity.
const INFO_KEY_EFFECTIVE_MIN_QUANTITY: &str = "minQuantityEffective";

/// The `info` key recording the system increment the minimum was normalized to.
const INFO_KEY_SYSTEM_SIZE_INCREMENT: &str = "minQuantitySystemIncrement";

/// The `info` key recording the human-readable normalization rule.
const INFO_KEY_NORMALIZATION_RULE: &str = "minQuantityNormalizationRule";

/// Native minimum order size from the IB contract-details response, normalized
/// into the whole-share domain the equity model can actually express.
///
/// C2.10-E2-R3-Q2.  The previous revision refused EVERY fractional native
/// minimum, which is correct about not rounding but wrong about the venue: real
/// equity contracts report a fractional `min_size` (live SPY reports `0.0001`)
/// together with an equally fractional `size_increment`, and refusing those
/// blocked every equity order outright.
///
/// The rule implemented here is a CEILING onto the system's one-share
/// increment, proven with exact decimal arithmetic:
///
/// 1. the native `min_size` must be present, finite and strictly positive;
/// 2. the native `size_increment` must be present, finite and strictly positive;
/// 3. both are restated EXACTLY as decimals (Rust's shortest round-tripping
///    `f64` rendering) and re-expressed at a common scale as integers, so all
///    subsequent arithmetic is exact integer arithmetic - no floating point and
///    no `Decimal` division appear anywhere below this point;
/// 4. the system's one-share increment must lie ON the venue's increment
///    lattice (`system % increment == 0`), otherwise alignment cannot be proven;
/// 5. the native minimum must itself be a whole number of venue increments,
///    otherwise the venue's own two fields disagree and nothing is inferred;
/// 6. the effective minimum is the smallest whole-share quantity at or ABOVE
///    the native minimum - a ceiling, never a nearest-value rounding, so the
///    published minimum can only ever narrow the venue's constraint;
/// 7. the result must be representable by the equity model at its own size
///    precision (`0`), and the share count must be small enough that an `f64`
///    reproduces it exactly;
/// 8. minimality and the lower bound are re-proved on the computed value rather
///    than assumed from the formula.
///
/// Every refusal publishes `None` and records WHY in
/// `info["minQuantitySource"]`.  Nothing is ever inferred from `lot_size` or
/// `suggested_size_increment`, `suggested_size_increment` is never treated as a
/// minimum or a mandatory increment, and no symbol-specific logic exists.
#[must_use]
fn equity_min_quantity(details: &ibapi::contracts::ContractDetails) -> EquityMinQuantity {
    let native_min = details.min_size;
    let native_increment = details.size_increment;

    // --- 1. the native minimum ------------------------------------------- //
    if native_min == 0.0 {
        // Covers both an unset protobuf field (the default `f64` is 0.0) and
        // an explicit zero, which are indistinguishable on the wire.
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_MISSING);
    }

    if !native_min.is_finite() {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_NON_FINITE);
    }

    if native_min < 0.0 {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_NON_POSITIVE);
    }

    // --- 2. the native increment ----------------------------------------- //
    if native_increment == 0.0 {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_INCREMENT_MISSING);
    }

    if !native_increment.is_finite() {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_INCREMENT_NON_FINITE);
    }

    if native_increment < 0.0 {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_INCREMENT_NON_POSITIVE);
    }

    // --- 3. exact decimal restatement ------------------------------------ //
    let (Some(min_dec), Some(increment_dec)) =
        (exact_decimal(native_min), exact_decimal(native_increment))
    else {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    };

    // --- 4. common EXACT integer scale ----------------------------------- //
    let scale = min_dec.scale().max(increment_dec.scale());
    let (Some(min_units), Some(increment_units)) = (
        mantissa_at_scale(min_dec, scale),
        mantissa_at_scale(increment_dec, scale),
    ) else {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    };
    let Some(system_units) =
        pow10_i128(scale).and_then(|unit| unit.checked_mul(EQUITY_SYSTEM_SIZE_INCREMENT_SHARES))
    else {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    };

    if min_units <= 0 || increment_units <= 0 || system_units <= 0 {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    }

    // --- 5. the system increment must lie on the venue lattice ----------- //
    if system_units % increment_units != 0 {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_INCREMENT_INCOMPATIBLE);
    }

    // --- 6. the native minimum must itself be on the venue lattice ------- //
    if min_units % increment_units != 0 {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_MIN_OFF_LATTICE);
    }

    // --- 7. the smallest whole-share count at or ABOVE the minimum ------- //
    let Some(shares) = ceiling_div(min_units, system_units) else {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    };

    if shares <= 0 {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    }

    let Some(effective_units) = shares.checked_mul(system_units) else {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    };

    // --- 8. re-prove the bound and the minimality of the result ---------- //
    if effective_units < min_units || effective_units - system_units >= min_units {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    }

    // The effective minimum, exactly, at the common scale.
    let effective = Decimal::from_i128_with_scale(effective_units, scale);

    // --- 9. representable by the model at the equity size precision ------ //
    let Some(effective_f64) = share_count_to_f64(shares) else {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    };

    let Ok(quantity) = Quantity::new_checked(effective_f64, EQUITY_SIZE_PRECISION) else {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    };

    if quantity.is_zero() {
        return EquityMinQuantity::refused(MIN_QUANTITY_SOURCE_UNREPRESENTABLE);
    }

    // The published minimum is the native value verbatim when the native value
    // was already a whole number of shares; otherwise it is a normalized
    // ceiling and says so.
    let source = if effective_units == min_units {
        MIN_QUANTITY_SOURCE_NATIVE
    } else {
        MIN_QUANTITY_SOURCE_NORMALIZED
    };

    EquityMinQuantity {
        quantity: Some(quantity),
        source,
        raw_min_size: Some(min_dec),
        raw_size_increment: Some(increment_dec),
        effective: Some(effective),
        system_increment: Decimal::from_i128_with_scale(EQUITY_SYSTEM_SIZE_INCREMENT_SHARES, 0),
    }
}

/// The outcome of resolving an equity minimum order quantity from the native
/// IB contract-details fields.
///
/// It carries both the resolved quantity and every input and output needed to
/// audit the decision, so the provenance recorded on the instrument distinguishes
/// the RAW venue minimum, the RAW venue increment, the EFFECTIVE system
/// minimum, the SYSTEM increment, and the rule that produced them.
#[derive(Debug)]
struct EquityMinQuantity {
    quantity: Option<Quantity>,
    source: &'static str,
    raw_min_size: Option<Decimal>,
    raw_size_increment: Option<Decimal>,
    effective: Option<Decimal>,
    system_increment: Decimal,
}

impl EquityMinQuantity {
    /// A refusal carries no quantity and no effective value, only the reason.
    fn refused(source: &'static str) -> Self {
        Self {
            quantity: None,
            source,
            raw_min_size: None,
            raw_size_increment: None,
            effective: None,
            system_increment: Decimal::from_i128_with_scale(EQUITY_SYSTEM_SIZE_INCREMENT_SHARES, 0),
        }
    }

    /// The provenance entries this decision contributes to `info`.
    fn provenance(&self) -> Vec<(&'static str, serde_json::Value)> {
        // `normalize()` only strips trailing zeros (it never changes the
        // value), so a minimum of one share reads as "1" rather than "1.0000"
        // whatever scale the venue's increment happened to require.
        let decimal_value = |value: Option<Decimal>| match value {
            Some(decimal) => serde_json::Value::from(decimal.normalize().to_string()),
            None => serde_json::Value::Null,
        };

        vec![
            (INFO_KEY_RAW_MIN_SIZE, decimal_value(self.raw_min_size)),
            (
                INFO_KEY_RAW_SIZE_INCREMENT,
                decimal_value(self.raw_size_increment),
            ),
            (
                INFO_KEY_EFFECTIVE_MIN_QUANTITY,
                decimal_value(self.effective),
            ),
            (
                INFO_KEY_SYSTEM_SIZE_INCREMENT,
                serde_json::Value::from(self.system_increment.to_string()),
            ),
            (
                INFO_KEY_NORMALIZATION_RULE,
                serde_json::Value::from(self.source),
            ),
        ]
    }
}

/// Restate a native `f64` from the IB wire as an EXACT [`Decimal`].
///
/// Rust's `Display` for `f64` emits the shortest decimal string that round-trips
/// back to the same `f64`, so parsing that string is a lossless restatement of
/// the value the venue actually sent.  `Decimal::from_f64` applies its own
/// rounding strategy and is deliberately not used; nothing here rounds.
///
/// Returns `None` when the value is not finite, or when its exact decimal form
/// exceeds what [`Decimal`] can hold - both of which mean the value cannot take
/// part in an exact proof and the caller must fail closed.
fn exact_decimal(value: f64) -> Option<Decimal> {
    if !value.is_finite() {
        return None;
    }

    Decimal::from_str(&format!("{value}")).ok()
}

/// Re-express `value` at `scale`, returning its EXACT `i128` mantissa.
///
/// Refuses rather than rounds when `value` carries more precision than `scale`,
/// and refuses on overflow.
fn mantissa_at_scale(value: Decimal, scale: u32) -> Option<i128> {
    let own_scale = value.scale();

    if own_scale > scale {
        return None;
    }

    value.mantissa().checked_mul(pow10_i128(scale - own_scale)?)
}

/// `10^exponent` as an exact `i128`, or `None` on overflow.
fn pow10_i128(exponent: u32) -> Option<i128> {
    let mut acc: i128 = 1;

    for _ in 0..exponent {
        acc = acc.checked_mul(10)?;
    }

    Some(acc)
}

/// Exact integer ceiling division for strictly positive operands.
///
/// `(a + b - 1) / b` computed with checked arithmetic, so it is stable on the
/// toolchain this crate builds with and cannot silently overflow into a wrong
/// (smaller) share count.
fn ceiling_div(numerator: i128, denominator: i128) -> Option<i128> {
    if numerator <= 0 || denominator <= 0 {
        return None;
    }

    numerator
        .checked_add(denominator)?
        .checked_sub(1)?
        .checked_div(denominator)
}

/// Convert an exact whole-share count into the `f64` the model accepts.
///
/// Beyond `2^53` an `f64` cannot represent every integer, so the count could not
/// be reproduced exactly and the caller must refuse instead of publishing a
/// value that silently differs from the one that was proved.
fn share_count_to_f64(shares: i128) -> Option<f64> {
    const MAX_EXACT_F64_INTEGER: i128 = 1i128 << 53;

    if shares <= 0 || shares > MAX_EXACT_F64_INTEGER {
        return None;
    }

    Some(shares as f64)
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

    // C2.10-E2-R2 / R1, corrected by C2.10-E2-R3-Q2: the equity minimum
    // executable quantity is propagated from the native IB contract-details
    // response, normalized from the venue's own `min_size` and `size_increment`
    // onto the system's whole-share increment.  The builder never set
    // `min_quantity` before the R1 correction, so `Equity::min_quantity` stayed
    // `None`, the instrument-legality gate downstream could not establish the
    // legality of any quantity, and EVERY equity request was refused before
    // submission.  The R1 correction propagated the native value but refused
    // every fractional one, which blocked live contracts such as SPY that
    // report `min_size = 0.0001`.  The normalization now in force ceilings the
    // native minimum onto the one-share lattice and records the full derivation.
    // When the native metadata is absent, inconsistent or unusable the field is
    // deliberately left absent again, so that gate refuses instead of a value
    // being invented or rounded.
    // `price_precision`, `price_increment`, `lot_size` (the round lot) and the
    // instrument identity are unchanged.
    let min_quantity = equity_min_quantity(details);

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
        serde_json::Value::from(min_quantity.source),
    );

    for (key, value) in min_quantity.provenance() {
        info.insert(key.to_string(), value);
    }

    let instrument = Equity::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(details.contract.local_symbol.as_str()))
        .currency(Currency::from(details.contract.currency.to_string()))
        .price_precision(price_precision)
        .price_increment(Price::new(details.min_tick, price_precision))
        // `None` (absent, inconsistent or unusable native metadata) leaves
        // `min_quantity` UNSET, which is the deliberate fail-closed outcome: the
        // downstream instrument-legality gate then refuses any quantity for this
        // instrument instead of a minimum being manufactured for it.
        .maybe_min_quantity(min_quantity.quantity)
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
    // C2.10-E2-R2-R1: the provenance labels the parser publishes, so the tests
    // assert the EXACT reason an unusable native minimum was not published.
    use std::str::FromStr;

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
    use rust_decimal::Decimal;
    use ustr::Ustr;

    use super::{
        INFO_KEY_EFFECTIVE_MIN_QUANTITY, INFO_KEY_NORMALIZATION_RULE, INFO_KEY_RAW_MIN_SIZE,
        INFO_KEY_RAW_SIZE_INCREMENT, INFO_KEY_SYSTEM_SIZE_INCREMENT,
        MIN_QUANTITY_SOURCE_INCREMENT_INCOMPATIBLE, MIN_QUANTITY_SOURCE_INCREMENT_MISSING,
        MIN_QUANTITY_SOURCE_INCREMENT_NON_FINITE, MIN_QUANTITY_SOURCE_INCREMENT_NON_POSITIVE,
        MIN_QUANTITY_SOURCE_MIN_OFF_LATTICE, MIN_QUANTITY_SOURCE_MISSING,
        MIN_QUANTITY_SOURCE_NATIVE, MIN_QUANTITY_SOURCE_NON_FINITE,
        MIN_QUANTITY_SOURCE_NON_POSITIVE, MIN_QUANTITY_SOURCE_NORMALIZED,
        MIN_QUANTITY_SOURCE_UNREPRESENTABLE, parse_contract_multiplier,
        parse_ib_contract_to_instrument, parse_option_spread_instrument_id,
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

    /// Requirements 8 and 9 (R1), RESTATED by C2.10-E2-R3-Q2: a native minimum
    /// that cannot be safely normalized must never be rounded into a whole
    /// share.  `Quantity::new(0.5, 0)` would silently yield 1 and
    /// `Quantity::new(1.5, 0)` would silently yield 2, because
    /// `f64_to_fixed_u128` rounds to the requested precision.  The R3-Q2 rule
    /// replaces that blanket refusal with a PROVEN ceiling, so each case here
    /// must be refused for an identifiable reason rather than by rounding.
    ///
    /// The default fixture carries `size_increment = 1.0`, so a native minimum
    /// with a fractional part is not a whole number of venue increments and the
    /// venue's own two fields disagree.  Nothing is inferred from inconsistent
    /// metadata, so these all still fail closed.
    #[rstest]
    #[case(0.5, MIN_QUANTITY_SOURCE_MIN_OFF_LATTICE)]
    #[case(1.5, MIN_QUANTITY_SOURCE_MIN_OFF_LATTICE)]
    #[case(2.25, MIN_QUANTITY_SOURCE_MIN_OFF_LATTICE)]
    #[case(0.999, MIN_QUANTITY_SOURCE_MIN_OFF_LATTICE)]
    #[case(f64::MIN_POSITIVE, MIN_QUANTITY_SOURCE_UNREPRESENTABLE)]
    fn test_parse_equity_contract_fractional_native_min_size_is_never_rounded(
        #[case] min_size: f64,
        #[case] expected_source: &str,
    ) {
        let (equity, _) = stock_equity("SPY", "ARCA", min_size);
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(
            equity.min_quantity(),
            None,
            "native min_size {min_size} must not be published for a \
             whole-share instrument"
        );
        // And specifically not the rounded whole share it would have become.
        assert_ne!(equity.min_quantity(), Some(Quantity::new(1.0, 0)));
        assert_ne!(equity.min_quantity(), Some(Quantity::new(2.0, 0)));
        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(expected_source)),
        );
        // No fractional-share support is claimed anywhere.
        assert_eq!(equity.size_precision(), 0);
        assert_eq!(
            info.get(INFO_KEY_EFFECTIVE_MIN_QUANTITY),
            Some(&serde_json::Value::Null),
            "a refusal publishes no effective minimum"
        );
    }

    // ------------------------------------------------------------------- //
    // C2.10-E2-R3-Q2: whole-share normalization of native fractional       //
    // equity constraints                                                   //
    // ------------------------------------------------------------------- //

    /// The required normalization table from the slice contract, executed
    /// against the real parser.  Every case is a CEILING onto the one-share
    /// increment, never a nearest-value rounding.
    #[rstest]
    #[case(0.0001, 0.0001, 1.0)]
    #[case(0.5, 0.0001, 1.0)]
    #[case(1.0, 0.0001, 1.0)]
    #[case(1.5, 0.0001, 2.0)]
    #[case(2.25, 0.0001, 3.0)]
    #[case(5.0, 0.0001, 5.0)]
    // The same table at other venue increments, all of which the system's
    // one-share increment divides exactly.
    #[case(0.0001, 0.000001, 1.0)]
    #[case(0.5, 0.5, 1.0)]
    #[case(1.5, 0.5, 2.0)]
    #[case(2.25, 0.25, 3.0)]
    #[case(1.0, 1.0, 1.0)]
    fn test_parse_equity_contract_normalizes_native_minimum_by_ceiling(
        #[case] min_size: f64,
        #[case] size_increment: f64,
        #[case] expected: f64,
    ) {
        let details = stock_details_with_increments("SPY", "ARCA", min_size, size_increment, 100.0);
        let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));
        let InstrumentAny::Equity(equity) =
            parse_ib_contract_to_instrument(&details, instrument_id).unwrap()
        else {
            panic!("expected equity");
        };
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(
            equity.min_quantity(),
            Some(Quantity::new(expected, 0)),
            "min {min_size} / increment {size_increment} must normalize to \
             {expected} whole shares"
        );
        assert_eq!(equity.min_quantity().unwrap().precision, 0);
        assert_eq!(equity.size_precision(), 0);
        assert_eq!(
            info.get(INFO_KEY_EFFECTIVE_MIN_QUANTITY),
            Some(&serde_json::Value::from(format!("{expected}"))),
        );
        assert_eq!(
            info.get(INFO_KEY_SYSTEM_SIZE_INCREMENT),
            Some(&serde_json::Value::from("1")),
        );
        // Raw venue metadata survives verbatim alongside the derivation.
        assert_eq!(
            info.get("minSize"),
            Some(&serde_json::Value::from(min_size)),
        );
        assert_eq!(
            info.get("sizeIncrement"),
            Some(&serde_json::Value::from(size_increment)),
        );
        assert_eq!(
            info.get("suggestedSizeIncrement"),
            Some(&serde_json::Value::from(100.0)),
        );
    }

    /// `minQuantitySource` must distinguish a minimum published VERBATIM from
    /// the native field from one produced by the ceiling.
    #[rstest]
    #[case(1.0, 1.0, MIN_QUANTITY_SOURCE_NATIVE)]
    #[case(5.0, 1.0, MIN_QUANTITY_SOURCE_NATIVE)]
    #[case(1.0, 0.0001, MIN_QUANTITY_SOURCE_NATIVE)]
    #[case(0.0001, 0.0001, MIN_QUANTITY_SOURCE_NORMALIZED)]
    #[case(0.5, 0.0001, MIN_QUANTITY_SOURCE_NORMALIZED)]
    #[case(1.5, 0.0001, MIN_QUANTITY_SOURCE_NORMALIZED)]
    #[case(2.25, 0.0001, MIN_QUANTITY_SOURCE_NORMALIZED)]
    fn test_parse_equity_contract_records_which_rule_published_the_minimum(
        #[case] min_size: f64,
        #[case] size_increment: f64,
        #[case] expected_source: &str,
    ) {
        let details = stock_details_with_increments("SPY", "ARCA", min_size, size_increment, 100.0);
        let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));
        let InstrumentAny::Equity(equity) =
            parse_ib_contract_to_instrument(&details, instrument_id).unwrap()
        else {
            panic!("expected equity");
        };
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(expected_source)),
        );
        assert_eq!(
            info.get(INFO_KEY_NORMALIZATION_RULE),
            Some(&serde_json::Value::from(expected_source)),
        );
        // The raw inputs are ALWAYS recorded, whatever the outcome.
        assert!(info.get(INFO_KEY_RAW_MIN_SIZE).is_some());
        assert!(info.get(INFO_KEY_RAW_SIZE_INCREMENT).is_some());
    }

    /// The normalization is a NARROWING: the published minimum is never below
    /// the native one, and is always the SMALLEST whole share at or above it.
    #[rstest]
    #[case(0.0001, 0.0001)]
    #[case(0.5, 0.0001)]
    #[case(1.0, 0.0001)]
    #[case(1.5, 0.0001)]
    #[case(2.25, 0.0001)]
    #[case(5.0, 0.0001)]
    #[case(2.25, 0.25)]
    fn test_parse_equity_contract_normalized_minimum_is_a_safe_ceiling(
        #[case] min_size: f64,
        #[case] size_increment: f64,
    ) {
        let details = stock_details_with_increments("SPY", "ARCA", min_size, size_increment, 100.0);
        let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));
        let InstrumentAny::Equity(equity) =
            parse_ib_contract_to_instrument(&details, instrument_id).unwrap()
        else {
            panic!("expected equity");
        };

        let published = equity.min_quantity().expect("minimum published");
        let published_decimal = Decimal::from_str(&published.to_string()).unwrap();
        let native_decimal = Decimal::from_str(&format!("{min_size}")).unwrap();

        assert!(
            published_decimal >= native_decimal,
            "the published minimum {published_decimal} must never be BELOW the \
             native minimum {native_decimal}"
        );
        assert!(
            published_decimal - Decimal::ONE < native_decimal,
            "the published minimum {published_decimal} must be the SMALLEST \
             whole share at or above the native minimum {native_decimal}"
        );
    }

    /// A native increment the system's one-share step cannot express as a whole
    /// number of increments proves no alignment, so nothing is published.
    #[rstest]
    #[case(1.0, 5.0)]
    #[case(5.0, 25.0)]
    #[case(1.0, 3.0)]
    #[case(2.0, 7.0)]
    #[case(1.0, 0.3)]
    // A 40-share lattice: one share is not a whole number of 40-share steps.
    #[case(40.0, 40.0)]
    fn test_parse_equity_contract_incompatible_venue_increment_fails_closed(
        #[case] min_size: f64,
        #[case] size_increment: f64,
    ) {
        let details = stock_details_with_increments("SPY", "ARCA", min_size, size_increment, 100.0);
        let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));
        let InstrumentAny::Equity(equity) =
            parse_ib_contract_to_instrument(&details, instrument_id).unwrap()
        else {
            panic!("expected equity");
        };
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(
            equity.min_quantity(),
            None,
            "the system one-share increment does not lie on the {size_increment} \
             lattice, so alignment cannot be proven"
        );
        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(
                MIN_QUANTITY_SOURCE_INCREMENT_INCOMPATIBLE
            )),
        );
    }

    /// A usable `min_size` cannot rescue an unusable increment: normalization
    /// needs the lattice, so a missing, non-finite or non-positive
    /// `size_increment` fails closed with its own provenance.
    #[rstest]
    #[case(0.0, MIN_QUANTITY_SOURCE_INCREMENT_MISSING)]
    #[case(-1.0, MIN_QUANTITY_SOURCE_INCREMENT_NON_POSITIVE)]
    #[case(f64::NAN, MIN_QUANTITY_SOURCE_INCREMENT_NON_FINITE)]
    #[case(f64::INFINITY, MIN_QUANTITY_SOURCE_INCREMENT_NON_FINITE)]
    #[case(f64::NEG_INFINITY, MIN_QUANTITY_SOURCE_INCREMENT_NON_FINITE)]
    fn test_parse_equity_contract_unusable_venue_increment_fails_closed(
        #[case] size_increment: f64,
        #[case] expected_source: &str,
    ) {
        let details = stock_details_with_increments("SPY", "ARCA", 1.0, size_increment, 100.0);
        let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));
        let InstrumentAny::Equity(equity) =
            parse_ib_contract_to_instrument(&details, instrument_id).unwrap()
        else {
            panic!("expected equity");
        };
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(equity.min_quantity(), None);
        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(expected_source)),
        );
    }

    /// `suggested_size_increment` is advisory only.  It must never become the
    /// minimum, never cap or raise the published minimum, and never be treated
    /// as a mandatory increment - including when it is the ONLY larger number
    /// in the response.
    #[rstest]
    #[case(0.0001, 0.0001, 40.0)]
    #[case(0.0001, 0.0001, 1.0e9)]
    #[case(1.0, 0.0001, 40.0)]
    #[case(1.5, 0.0001, 500.0)]
    fn test_parse_equity_contract_suggested_increment_never_becomes_the_minimum(
        #[case] min_size: f64,
        #[case] size_increment: f64,
        #[case] suggested: f64,
    ) {
        let details =
            stock_details_with_increments("SPY", "ARCA", min_size, size_increment, suggested);
        let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));
        let InstrumentAny::Equity(equity) =
            parse_ib_contract_to_instrument(&details, instrument_id).unwrap()
        else {
            panic!("expected equity");
        };
        let info = equity.info.clone().expect("instrument info");

        let published = equity.min_quantity().expect("minimum published");
        assert_ne!(published, Quantity::new(suggested, 0));
        assert!(
            Decimal::from_str(&published.to_string()).unwrap()
                < Decimal::from_str(&format!("{suggested}")).unwrap(),
            "the published minimum must not be inflated to the suggested size \
             increment"
        );
        assert_eq!(
            info.get("suggestedSizeIncrement"),
            Some(&serde_json::Value::from(suggested)),
        );
    }

    /// The exact live SPY fixture collected in C2.10-E2-Q1-R1: the real TWS
    /// PAPER contract details that blocked every order.  This is the case the
    /// correction exists for.
    #[rstest]
    fn test_parse_equity_contract_live_spy_fractional_fixture_normalizes_to_one_share() {
        let mut details = stock_details_with_increments("SPY", "ARCA", 0.0001, 0.0001, 40.0);
        details.min_tick = 0.01;
        details.contract.local_symbol = "SPY".to_string();

        let instrument_id = InstrumentId::new(NautilusSymbol::from("SPY"), Venue::from("ARCA"));
        let InstrumentAny::Equity(equity) =
            parse_ib_contract_to_instrument(&details, instrument_id).unwrap()
        else {
            panic!("expected equity");
        };
        let info = equity.info.clone().expect("instrument info");

        assert_eq!(
            equity.min_quantity(),
            Some(Quantity::new(1.0, 0)),
            "live SPY minSize 0.0001 / sizeIncrement 0.0001 must normalize to \
             exactly one whole share"
        );
        assert_eq!(equity.id, instrument_id);
        assert_eq!(equity.size_precision(), 0);
        assert_eq!(equity.lot_size(), Some(Quantity::new(100.0, 0)));
        assert_eq!(equity.price_precision(), 2);
        assert_eq!(equity.price_increment(), Price::new(0.01, 2));

        // Full provenance: raw in, effective out, rule that produced it.
        assert_eq!(info.get("minSize"), Some(&serde_json::Value::from(0.0001)));
        assert_eq!(
            info.get("sizeIncrement"),
            Some(&serde_json::Value::from(0.0001)),
        );
        assert_eq!(
            info.get("suggestedSizeIncrement"),
            Some(&serde_json::Value::from(40.0)),
        );
        assert_eq!(
            info.get(INFO_KEY_RAW_MIN_SIZE),
            Some(&serde_json::Value::from("0.0001")),
        );
        assert_eq!(
            info.get(INFO_KEY_RAW_SIZE_INCREMENT),
            Some(&serde_json::Value::from("0.0001")),
        );
        assert_eq!(
            info.get(INFO_KEY_EFFECTIVE_MIN_QUANTITY),
            Some(&serde_json::Value::from("1")),
        );
        assert_eq!(
            info.get(INFO_KEY_SYSTEM_SIZE_INCREMENT),
            Some(&serde_json::Value::from("1")),
        );
        assert_eq!(
            info.get("minQuantitySource"),
            Some(&serde_json::Value::from(MIN_QUANTITY_SOURCE_NORMALIZED)),
        );
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
    ///
    /// RESTATED by C2.10-E2-R3-Q2: `size_increment` is now a REQUIRED input to
    /// the normalization (it supplies the lattice), but it is still never the
    /// published minimum.  The cases below use increments the system's
    /// one-share step divides exactly, so a minimum IS published - and it is the
    /// ceiling, not either increment.
    #[rstest]
    #[case(1.0, 0.25, 250.0, 1.0)]
    #[case(5.0, 0.25, 500.0, 5.0)]
    #[case(1.5, 0.5, 500.0, 2.0)]
    fn test_parse_equity_contract_size_increments_are_not_substituted(
        #[case] min_size: f64,
        #[case] size_increment: f64,
        #[case] suggested_size_increment: f64,
        #[case] expected: f64,
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

        // The published minimum is the ceiling of the native `min_size`, not
        // either increment.
        assert_eq!(equity.min_quantity(), Some(Quantity::new(expected, 0)));
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
    ///
    /// The pre-R3-Q2 form of this test used `size_increment = 1.0` with a zero
    /// minimum, so the minimum was rejected before the increment was ever
    /// consulted.  The increment is now itself a required input, so the
    /// increment is held at its default and only the minimum varies - which is
    /// what "the increments did not rescue it" actually means.
    #[rstest]
    #[case(0.0)]
    #[case(-1.0)]
    #[case(f64::NAN)]
    #[case(f64::INFINITY)]
    fn test_parse_equity_contract_increments_do_not_rescue_unusable_minimum(#[case] min_size: f64) {
        let details = stock_details_with_increments("SPY", "ARCA", min_size, 1.0, 100.0);
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
