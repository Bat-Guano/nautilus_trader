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

//! Python bindings for inbound IBKR order parsing, classification and refusal
//! (CWR-01B-O3-R1, CWR-01B-O3-R2).
//!
//! Each binding projects a decision which already exists in Rust and is already
//! applied on the inbound paths: [`classify_ib_order_type`] classifies a wire
//! value, [`project_ib_order_type`] resolves the canonical projection or refuses
//! it, [`IbOrderTypeRefusal`] carries that refusal, [`is_usable_ib_price`] guards
//! broker price fields, [`ib_open_order_price_shape`] is the S2 open-order price
//! decision, and [`parse_order_status_to_report`] is the S1 inbound order-report
//! parser itself. Nothing here restates a rule, so a Python consumer cannot
//! disagree with the adapter about what an inbound order means.

use std::str::FromStr;

use ibapi::orders::{Action, Order, OrderStatus, OrderStatusKind, TimeInForce};
use nautilus_core::UnixNanos;
use nautilus_model::{
    enums::OrderType,
    identifiers::{AccountId, InstrumentId},
    types::Price,
};
use pyo3::{
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
    types::PyDict,
};

use crate::{
    common::{
        enums::{
            IbOrderTypeClass, IbOrderTypeRefusal, classify_ib_order_type, project_ib_order_type,
        },
        parse::is_usable_ib_price,
    },
    config::InteractiveBrokersInstrumentProviderConfig,
    execution::parse::{ib_open_order_price_shape, parse_order_status_to_report},
    providers::instruments::InteractiveBrokersInstrumentProvider,
};

/// The five classes of [`IbOrderTypeClass`], as a Python-visible enum.
///
/// [`IbOrderTypeClass`] itself cannot be bound to Python: PyO3's complex-enum
/// support has no representation for an enum mixing payload-carrying variants
/// (`Exact` and `Partial` carry the canonical type) with unit variants (the
/// three refused classes), and the reviewed classifier keeps that shape. This
/// type is therefore the classification *view* and decides nothing. Both
/// directions of the mapping match [`IbOrderTypeClass`] exhaustively, so a class
/// added to or removed from the reviewed classifier fails to compile here rather
/// than diverging silently.
#[pyclass(
    name = "IbOrderTypeClass",
    module = "nautilus_trader.adapters.interactive_brokers",
    from_py_object,
    eq,
    frozen,
    rename_all = "SCREAMING_SNAKE_CASE"
)]
#[pyo3_stub_gen::derive::gen_stub_pyclass_enum(
    module = "nautilus_trader.adapters.interactive_brokers"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyIbOrderTypeClass {
    /// A recognized IBKR type whose canonical mapping is exact.
    Exact,
    /// A recognized IBKR type whose canonical mapping is truthful but loses an
    /// IBKR-specific attribute.
    Partial,
    /// A recognized IBKR type with no truthful canonical counterpart.
    Unsupported,
    /// A wire value outside the recognized IBKR order-type vocabulary.
    Unknown,
    /// An empty or whitespace-only wire value.
    Malformed,
}

impl PyIbOrderTypeClass {
    /// Returns the reviewed Rust class this view names.
    ///
    /// `Exact` and `Partial` carry a canonical type in [`IbOrderTypeClass`], but
    /// neither [`IbOrderTypeClass::as_str`] nor [`IbOrderTypeClass::canonical`]
    /// reads it, so a representative payload names the class exactly.
    #[must_use]
    const fn as_class(self) -> IbOrderTypeClass {
        match self {
            Self::Exact => IbOrderTypeClass::Exact(OrderType::Market),
            Self::Partial => IbOrderTypeClass::Partial(OrderType::Market),
            Self::Unsupported => IbOrderTypeClass::Unsupported,
            Self::Unknown => IbOrderTypeClass::Unknown,
            Self::Malformed => IbOrderTypeClass::Malformed,
        }
    }
}

impl From<IbOrderTypeClass> for PyIbOrderTypeClass {
    fn from(class: IbOrderTypeClass) -> Self {
        match class {
            IbOrderTypeClass::Exact(_) => Self::Exact,
            IbOrderTypeClass::Partial(_) => Self::Partial,
            IbOrderTypeClass::Unsupported => Self::Unsupported,
            IbOrderTypeClass::Unknown => Self::Unknown,
            IbOrderTypeClass::Malformed => Self::Malformed,
        }
    }
}

#[pymethods]
#[pyo3_stub_gen::derive::gen_stub_pymethods]
impl PyIbOrderTypeClass {
    /// Returns the stable class label produced by the Rust classifier.
    #[pyo3(name = "as_str")]
    fn py_as_str(&self) -> &'static str {
        self.as_class().as_str()
    }

    /// Returns whether the class justifies no canonical order type.
    #[pyo3(name = "is_refused")]
    fn py_is_refused(&self) -> bool {
        self.as_class().canonical().is_none()
    }
}

#[pymethods]
#[pyo3_stub_gen::derive::gen_stub_pymethods]
impl IbOrderTypeRefusal {
    /// The exact raw IBKR order-type wire value.
    #[getter]
    #[pyo3(name = "raw_order_type")]
    fn py_raw_order_type(&self) -> String {
        self.raw_order_type.clone()
    }

    /// The classification which justifies no canonical order type.
    #[getter]
    #[pyo3(name = "classification")]
    fn py_classification(&self) -> PyIbOrderTypeClass {
        self.classification.into()
    }

    /// The explicit refusal reason, identical to the inbound path's refusal.
    #[getter]
    #[pyo3(name = "reason")]
    fn py_reason(&self) -> String {
        self.to_string()
    }

    /// The broker API order id, when one is known.
    #[getter]
    #[pyo3(name = "order_id")]
    fn py_order_id(&self) -> Option<i64> {
        self.order_id
    }

    /// The broker permanent order id, when one is known.
    #[getter]
    #[pyo3(name = "perm_id")]
    fn py_perm_id(&self) -> Option<i64> {
        self.perm_id
    }

    /// Returns a dict representation of this refusal.
    ///
    /// # Errors
    ///
    /// Returns a Python exception if conversion to dict fails.
    #[pyo3(name = "to_dict")]
    fn py_to_dict(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let dict = PyDict::new(py);
        dict.set_item("raw_order_type", &self.raw_order_type)?;
        dict.set_item(
            "classification",
            Py::new(py, PyIbOrderTypeClass::from(self.classification))?,
        )?;
        dict.set_item("reason", self.to_string())?;
        dict.set_item("order_id", self.order_id)?;
        dict.set_item("perm_id", self.perm_id)?;

        Ok(dict.into())
    }

    fn __repr__(&self) -> String {
        format!(
            "IbOrderTypeRefusal(raw_order_type={:?}, classification={}, order_id={:?}, perm_id={:?})",
            self.raw_order_type,
            self.classification.as_str(),
            self.order_id,
            self.perm_id,
        )
    }

    fn __str__(&self) -> String {
        self.to_string()
    }
}

/// Classifies an inbound IBKR order-type wire value.
///
/// The wire value is compared byte-for-byte after the client's transport-level
/// decoding: no case folding, trimming or alias rewriting is applied.
#[pyfunction]
#[pyo3_stub_gen::derive::gen_stub_pyfunction(
    module = "nautilus_trader.adapters.interactive_brokers"
)]
#[pyo3(name = "classify_ib_order_type")]
#[pyo3(signature = (raw_order_type, limit_price=None))]
pub fn py_classify_ib_order_type(
    raw_order_type: &str,
    limit_price: Option<f64>,
) -> PyIbOrderTypeClass {
    classify_ib_order_type(raw_order_type, limit_price).into()
}

/// Resolves the canonical projection for an inbound IBKR order type.
///
/// Returns the canonical `OrderType` when the wire value is faithfully
/// representable, and the `IbOrderTypeRefusal` naming why it is not otherwise.
/// This is the decision the inbound order-report path makes before it publishes
/// a report: no canonical `Market`, `Limit` or other type is ever substituted
/// for semantics the wire value does not carry.
#[pyfunction]
#[pyo3_stub_gen::derive::gen_stub_pyfunction(
    module = "nautilus_trader.adapters.interactive_brokers"
)]
#[pyo3(name = "project_ib_order_type")]
#[pyo3(signature = (raw_order_type, limit_price=None, order_id=None, perm_id=None))]
pub fn py_project_ib_order_type(
    py: Python<'_>,
    raw_order_type: &str,
    limit_price: Option<f64>,
    order_id: Option<i64>,
    perm_id: Option<i64>,
) -> PyResult<Py<PyAny>> {
    match project_ib_order_type(raw_order_type, limit_price, order_id, perm_id) {
        Ok(order_type) => Ok(Py::new(py, order_type)?.into_any()),
        Err(refusal) => Ok(Py::new(py, refusal)?.into_any()),
    }
}

/// Returns whether an IBKR price field carries a usable order price.
///
/// The reviewed predicate is `value.is_finite() && value > 0.0`: an IBKR price
/// field of zero, negative, NaN or infinity is an unset sentinel, not price
/// state, and must never become a `Price`.
#[pyfunction]
#[pyo3_stub_gen::derive::gen_stub_pyfunction(
    module = "nautilus_trader.adapters.interactive_brokers"
)]
#[pyo3(name = "is_usable_ib_price")]
pub fn py_is_usable_ib_price(value: f64) -> bool {
    is_usable_ib_price(value)
}

/// Returns the `(price, trigger_price)` shape for an IBKR open-order update.
///
/// The shape is selected by the canonical type when the wire value justifies
/// one; otherwise only the prices IBKR actually reported are carried, with the
/// same usable-price guard as the report path. Exported so the shape the
/// adapter sends for a wire value can be observed without a broker connection.
#[pyfunction]
#[pyo3_stub_gen::derive::gen_stub_pyfunction(
    module = "nautilus_trader.adapters.interactive_brokers"
)]
#[pyo3(name = "ib_open_order_price_shape")]
#[pyo3(signature = (raw_order_type, limit_price=None, aux_price=None, price_magnifier=1.0, price_precision=0, order_id=None))]
#[expect(clippy::too_many_arguments)]
pub fn py_ib_open_order_price_shape(
    raw_order_type: &str,
    limit_price: Option<f64>,
    aux_price: Option<f64>,
    price_magnifier: f64,
    price_precision: u8,
    order_id: Option<i32>,
) -> (Option<Price>, Option<Price>) {
    ib_open_order_price_shape(
        raw_order_type,
        limit_price,
        aux_price,
        price_magnifier,
        price_precision,
        order_id,
    )
}

/// Runs the actual inbound IBKR order-status parser on a deterministic fixture.
///
/// This binding exists to make the production inbound parser
/// ([`parse_order_status_to_report`], S1 in the adapter) executable from an
/// offline Python process. It builds the genuine rust-ibapi
/// `ibapi::orders::Order` and `ibapi::orders::OrderStatus` values the inbound
/// path consumes from the supplied IBKR wire fields, then calls the parser with
/// an empty, offline instrument provider (no broker, no cache, no connection).
///
/// Returns the parser's real `OrderStatusReport` when the broker evidence
/// supports a canonical report. When the wire order type cannot be truthfully
/// projected, the parser refuses; the same decision is taken here through
/// [`project_ib_order_type`] (the function S1 itself calls, with the same raw
/// value and the same identifiers), and the typed [`IbOrderTypeRefusal`] is
/// returned after checking that the parser's own refusal text is that value's
/// text. A parser failure for any other reason is raised as an exception — it is
/// never converted into a canonical order type or into a type refusal.
///
/// The raw wire strings (`order_type`, `status`) are preserved byte-for-byte and
/// reach the report as `raw_order_type` / `raw_order_status`. Order identity
/// (`order_id`, `perm_id`) is carried into the refusal so a refusal stays
/// correlatable with the broker order it belongs to.
///
/// # Errors
///
/// Raises `ValueError` for an input the inbound wire decoder could not produce
/// (an out-of-vocabulary action, an empty order status, an unparsable instrument
/// or account id), and `RuntimeError` if the parser fails for a reason other than
/// the order-type projection.
#[pyfunction]
#[pyo3_stub_gen::derive::gen_stub_pyfunction(
    module = "nautilus_trader.adapters.interactive_brokers"
)]
#[pyo3(name = "parse_ib_order_status_report")]
#[pyo3(signature = (
    order_type,
    total_quantity,
    instrument_id,
    account_id,
    ts_init,
    action="BUY",
    limit_price=None,
    aux_price=None,
    trail_stop_price=None,
    trailing_percent=None,
    limit_price_offset=None,
    tif="DAY",
    good_till_date="",
    order_ref="",
    order_id=0,
    perm_id=0,
    status="Submitted",
    filled=0.0,
    remaining=0.0,
    average_fill_price=None,
))]
#[expect(clippy::too_many_arguments)]
pub fn py_parse_ib_order_status_report(
    py: Python<'_>,
    order_type: &str,
    total_quantity: f64,
    instrument_id: &str,
    account_id: &str,
    ts_init: u64,
    action: &str,
    limit_price: Option<f64>,
    aux_price: Option<f64>,
    trail_stop_price: Option<f64>,
    trailing_percent: Option<f64>,
    limit_price_offset: Option<f64>,
    tif: &str,
    good_till_date: &str,
    order_ref: &str,
    order_id: i32,
    perm_id: i64,
    status: &str,
    filled: f64,
    remaining: f64,
    average_fill_price: Option<f64>,
) -> PyResult<Py<PyAny>> {
    // Decode the action exactly as the inbound wire vocabulary defines it. The
    // rust-ibapi `Action::from(&str)` is `todo!()`-unreachable out of vocabulary,
    // so decoding here keeps a malformed fixture observable instead of panicking.
    let action = match action {
        "BUY" => Action::Buy,
        "SELL" => Action::Sell,
        "SSHORT" => Action::SellShort,
        "SLONG" => Action::SellLong,
        other => {
            return Err(PyValueError::new_err(format!(
                "unrecognized IBKR order action {other:?}"
            )));
        }
    };

    // The genuine production `Order` value the inbound parser consumes. Every
    // field the parser reads is set from the fixture; the rest keep the type's
    // documented defaults, exactly as the reviewed Rust contract tests do.
    let order = Order {
        action,
        total_quantity,
        order_type: order_type.to_owned(),
        limit_price,
        aux_price,
        tif: TimeInForce::from(tif),
        good_till_date: good_till_date.to_owned(),
        order_ref: order_ref.to_owned(),
        trail_stop_price,
        trailing_percent,
        limit_price_offset,
        ..Default::default()
    };

    // The production wire decoder: known statuses map to their reviewed value,
    // an unknown non-empty wire value is preserved byte-for-byte, and only the
    // empty string (a missing required wire field) is an error.
    let status = OrderStatusKind::from_str(status).map_err(|e| {
        PyValueError::new_err(format!(
            "unrecognized IBKR order status wire value {status:?}: {e}"
        ))
    })?;

    let order_status = OrderStatus {
        order_id,
        status,
        filled,
        remaining,
        average_fill_price,
        perm_id,
        ..Default::default()
    };

    let instrument_id = InstrumentId::from_str(instrument_id).map_err(|e| {
        PyValueError::new_err(format!("invalid instrument id {instrument_id:?}: {e}"))
    })?;
    let account_id = AccountId::new_checked(account_id)
        .map_err(|e| PyValueError::new_err(format!("invalid account id {account_id:?}: {e}")))?;

    // Offline, empty provider: no broker connection, no instrument cache. The
    // parser therefore uses its documented defaults (price magnifier 1, zero
    // precision) exactly as the reviewed Rust contract tests do.
    let provider = InteractiveBrokersInstrumentProvider::new(
        InteractiveBrokersInstrumentProviderConfig::default(),
    );

    // The identical decision S1 takes over the same evidence and identifiers
    // (`parse.rs` passes `order.limit_price`, the API order id and the perm id).
    let projection = project_ib_order_type(
        &order.order_type,
        order.limit_price,
        Some(i64::from(order_status.order_id)),
        Some(order_status.perm_id),
    );

    match parse_order_status_to_report(
        &order_status,
        Some(&order),
        instrument_id,
        account_id,
        &provider,
        UnixNanos::new(ts_init),
    ) {
        Ok(report) => Ok(Py::new(py, report)?.into_any()),
        Err(err) => match projection {
            // S1 refused for the order type. Return the same typed refusal value,
            // but only after proving S1's own text is that refusal: a different
            // parser failure must never be disguised as a type refusal.
            Err(refusal) => {
                let parser_reason = err.to_string();
                let refusal_reason = refusal.to_string();
                if parser_reason != refusal_reason {
                    return Err(PyRuntimeError::new_err(format!(
                        "IBKR inbound parser refused for a reason other than the order-type projection: {parser_reason}"
                    )));
                }

                Ok(Py::new(py, refusal)?.into_any())
            }
            Ok(_) => Err(PyRuntimeError::new_err(err.to_string())),
        },
    }
}
