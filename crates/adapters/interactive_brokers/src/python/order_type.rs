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

//! Python bindings for inbound IBKR order-type classification and refusal (CWR-01B-O3-R1).
//!
//! Each binding projects a decision which already exists in Rust and is already
//! applied on the inbound paths: [`classify_ib_order_type`] classifies a wire
//! value, [`project_ib_order_type`] resolves the canonical projection or refuses
//! it, [`IbOrderTypeRefusal`] carries that refusal, [`is_usable_ib_price`] guards
//! broker price fields, and [`ib_open_order_price_shape`] is the S2 open-order
//! price decision. Nothing here restates a rule, so a Python consumer cannot
//! disagree with the adapter about what an inbound order means.

use nautilus_model::{enums::OrderType, types::Price};
use pyo3::{prelude::*, types::PyDict};

use crate::{
    common::{
        enums::{
            IbOrderTypeClass, IbOrderTypeRefusal, classify_ib_order_type, project_ib_order_type,
        },
        parse::is_usable_ib_price,
    },
    execution::parse::ib_open_order_price_shape,
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
