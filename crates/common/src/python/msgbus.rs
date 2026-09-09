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

//! Python bindings for the message bus, including configuration types and the
//! [`PyMessageBus`] wrapper that routes Python events through the Rust
//! thread-local [`MessageBus`] via the Any-based dispatch path.

use std::{any::Any, fmt::Debug, rc::Rc, sync::LazyLock};

use ahash::AHashMap;
use nautilus_core::{
    UUID4,
    python::{to_pyruntime_err, to_pyvalue_err},
};
use nautilus_model::{identifiers::TraderId, reports::OrderStatusReport};
use pyo3::{IntoPyObjectExt, Py, Python, prelude::*, types::PyBytes};
use ustr::Ustr;

use crate::{
    enums::SerializationEncoding,
    msgbus::{
        self as msgbus_api, BusMessage, MessageBus, MessageBusBackingFactory, MessageBusConfig,
        core::Subscription,
        get_message_bus,
        matching::is_matching,
        mstr::{Endpoint, MStr, Pattern, Topic},
        typed_handler::{Handler, ShareableMessageHandler, TypedHandler},
    },
    python::{
        config_error_to_pyvalue_err,
        factory::{FactoryExtractor, FactoryRegistry},
    },
};

/// Function type for extracting a Python object into a boxed message bus backing factory.
pub type MessageBusFactoryExtractor = FactoryExtractor<dyn MessageBusBackingFactory>;

/// Registry for Python message bus backing factory extractors.
#[derive(Debug)]
pub struct MessageBusFactoryRegistry {
    inner: FactoryRegistry<dyn MessageBusBackingFactory>,
}

impl MessageBusFactoryRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: FactoryRegistry::new("message bus factory"),
        }
    }

    // panics-doc-ok (transitive via FactoryRegistry mutex locking)
    /// Registers an extractor for a Python factory type name.
    ///
    /// # Errors
    ///
    /// Returns an error if a different extractor is already registered for the type name.
    pub fn register(
        &self,
        type_name: String,
        extractor: MessageBusFactoryExtractor,
    ) -> anyhow::Result<()> {
        self.inner.register(type_name, extractor)
    }

    // panics-doc-ok (transitive via FactoryRegistry mutex locking)
    /// Extracts a Python object into a boxed message bus backing factory.
    ///
    /// # Errors
    ///
    /// Returns an error if no extractor is registered for the Python type or extraction fails.
    pub fn extract(
        &self,
        py: Python<'_>,
        factory: Py<PyAny>,
    ) -> PyResult<Box<dyn MessageBusBackingFactory>> {
        self.inner.extract(py, factory)
    }
}

impl Default for MessageBusFactoryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

static GLOBAL_MSGBUS_FACTORY_REGISTRY: LazyLock<MessageBusFactoryRegistry> =
    LazyLock::new(MessageBusFactoryRegistry::new);

/// Returns the global Python message bus backing factory registry.
#[must_use]
pub fn get_global_msgbus_factory_registry() -> &'static MessageBusFactoryRegistry {
    &GLOBAL_MSGBUS_FACTORY_REGISTRY
}

#[pymethods]
#[pyo3_stub_gen::derive::gen_stub_pymethods]
impl BusMessage {
    #[getter]
    #[pyo3(name = "topic")]
    fn py_topic(&self) -> String {
        self.topic.to_string()
    }

    #[getter]
    #[pyo3(name = "payload_type")]
    fn py_payload_type(&self) -> String {
        self.payload_type.to_string()
    }

    #[getter]
    #[pyo3(name = "payload")]
    fn py_payload(&self, py: Python<'_>) -> Py<PyBytes> {
        PyBytes::new(py, self.payload.as_ref()).into()
    }

    #[getter]
    #[pyo3(name = "encoding")]
    fn py_encoding(&self) -> SerializationEncoding {
        self.encoding
    }

    fn __repr__(&self) -> String {
        format!("{}('{}')", stringify!(BusMessage), self)
    }

    fn __str__(&self) -> String {
        self.to_string()
    }
}

#[pymethods]
#[pyo3_stub_gen::derive::gen_stub_pymethods]
impl MessageBusConfig {
    /// Configuration for `MessageBus` instances.
    #[new]
    #[expect(clippy::too_many_arguments)]
    #[pyo3(signature = (encoding=None, encoding_market_data=None, encoding_builtin=None, timestamps_as_iso8601=None, buffer_interval_ms=None, autotrim_mins=None, autotrim_maxlen=None, use_trader_prefix=None, use_trader_id=None, use_instance_id=None, streams_prefix=None, stream_per_topic=None, external_streams=None, types_filter=None, heartbeat_interval_secs=None))]
    fn py_new(
        encoding: Option<SerializationEncoding>,
        encoding_market_data: Option<SerializationEncoding>,
        encoding_builtin: Option<SerializationEncoding>,
        timestamps_as_iso8601: Option<bool>,
        buffer_interval_ms: Option<u32>,
        autotrim_mins: Option<u32>,
        autotrim_maxlen: Option<u32>,
        use_trader_prefix: Option<bool>,
        use_trader_id: Option<bool>,
        use_instance_id: Option<bool>,
        streams_prefix: Option<String>,
        stream_per_topic: Option<bool>,
        external_streams: Option<Vec<String>>,
        types_filter: Option<Vec<String>>,
        heartbeat_interval_secs: Option<u16>,
    ) -> PyResult<Self> {
        let default = Self::default();
        let config = Self {
            encoding: encoding.unwrap_or(default.encoding),
            encoding_market_data,
            encoding_builtin,
            timestamps_as_iso8601: timestamps_as_iso8601.unwrap_or(default.timestamps_as_iso8601),
            buffer_interval_ms,
            autotrim_mins,
            autotrim_maxlen,
            use_trader_prefix: use_trader_prefix.unwrap_or(default.use_trader_prefix),
            use_trader_id: use_trader_id.unwrap_or(default.use_trader_id),
            use_instance_id: use_instance_id.unwrap_or(default.use_instance_id),
            streams_prefix: streams_prefix.unwrap_or(default.streams_prefix),
            stream_per_topic: stream_per_topic.unwrap_or(default.stream_per_topic),
            external_streams,
            types_filter,
            heartbeat_interval_secs,
        };

        config.validate().map_err(config_error_to_pyvalue_err)?;
        Ok(config)
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }

    fn __str__(&self) -> String {
        format!("{self:?}")
    }

    #[getter]
    fn encoding(&self) -> SerializationEncoding {
        self.encoding
    }

    #[getter]
    fn encoding_market_data(&self) -> Option<SerializationEncoding> {
        self.encoding_market_data
    }

    #[getter]
    fn encoding_builtin(&self) -> Option<SerializationEncoding> {
        self.encoding_builtin
    }

    #[getter]
    fn timestamps_as_iso8601(&self) -> bool {
        self.timestamps_as_iso8601
    }

    #[getter]
    fn buffer_interval_ms(&self) -> Option<u32> {
        self.buffer_interval_ms
    }

    #[getter]
    fn autotrim_mins(&self) -> Option<u32> {
        self.autotrim_mins
    }

    #[getter]
    fn autotrim_maxlen(&self) -> Option<u32> {
        self.autotrim_maxlen
    }

    #[getter]
    fn use_trader_prefix(&self) -> bool {
        self.use_trader_prefix
    }

    #[getter]
    fn use_trader_id(&self) -> bool {
        self.use_trader_id
    }

    #[getter]
    fn use_instance_id(&self) -> bool {
        self.use_instance_id
    }

    #[getter]
    fn streams_prefix(&self) -> &str {
        &self.streams_prefix
    }

    #[getter]
    fn stream_per_topic(&self) -> bool {
        self.stream_per_topic
    }

    #[getter]
    fn external_streams(&self) -> Option<Vec<String>> {
        self.external_streams.clone()
    }

    #[getter]
    fn types_filter(&self) -> Option<Vec<String>> {
        self.types_filter.clone()
    }

    #[getter]
    fn heartbeat_interval_secs(&self) -> Option<u16> {
        self.heartbeat_interval_secs
    }
}

/// Wraps a Python object so it can travel through the Rust Any-based message bus.
pub struct PyMessage(pub Py<PyAny>);

impl Debug for PyMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple(stringify!(PyMessage))
            .field(&"<PyObject>")
            .finish()
    }
}

/// Adapts a Python callable as a [`ShareableMessageHandler`].
///
/// Dispatches [`PyMessage`] payloads (the Python publish path) and genuine
/// Rust [`OrderStatusReport`] values published through the same Any-based
/// `publish_any` path used by `ExecutionEngine`. Other Rust types remain
/// unsupported and are logged, matching the previous non-`PyMessage` behavior.
/// Acquires the GIL and calls the Python callable with a Python object.
pub struct PyCallableHandler {
    id: Ustr,
    callable: Py<PyAny>,
}

impl Debug for PyCallableHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(PyCallableHandler))
            .field("id", &self.id)
            .finish()
    }
}

impl PyCallableHandler {
    /// Creates a new handler from a Python callable.
    ///
    /// The handler ID is derived from `repr(callable)` for stable identity
    /// across subscribe/unsubscribe calls.
    pub fn new(py: Python<'_>, callable: Py<PyAny>) -> PyResult<Self> {
        let repr_str = callable.bind(py).repr()?.to_string();
        let id = Ustr::from(&repr_str);
        Ok(Self { id, callable })
    }
}

impl Handler<dyn Any> for PyCallableHandler {
    fn id(&self) -> Ustr {
        self.id
    }

    fn handle(&self, message: &dyn Any) {
        Python::attach(|py| {
            let py_obj = if let Some(py_msg) = message.downcast_ref::<PyMessage>() {
                py_msg.0.clone_ref(py)
            } else if let Some(report) = message.downcast_ref::<OrderStatusReport>() {
                match report.clone().into_py_any(py) {
                    Ok(obj) => obj,
                    Err(e) => {
                        log::error!(
                            "Python handler {id} failed to convert OrderStatusReport: {e}",
                            id = self.id
                        );
                        return;
                    }
                }
            } else {
                log::error!(
                    "Python handler {id} received non-PyMessage type",
                    id = self.id
                );
                return;
            };

            if let Err(e) = self.callable.call1(py, (&py_obj,)) {
                log::error!("Python handler {id} failed: {e}", id = self.id);
            }
        });
    }
}

fn make_handler(py: Python<'_>, callable: Py<PyAny>) -> PyResult<ShareableMessageHandler> {
    let handler = PyCallableHandler::new(py, callable)?;
    Ok(TypedHandler(Rc::new(handler) as Rc<dyn Handler<dyn Any>>))
}

/// Python message bus backed by the Rust thread-local [`MessageBus`].
///
/// Publish, subscribe, and request/response calls from Python route through the
/// single Rust bus. Python custom events travel through the Any-based dispatch
/// path via [`PyMessage`] wrappers.
#[pyclass(module = "nautilus_trader.common", name = "MessageBus", unsendable)]
#[pyo3_stub_gen::derive::gen_stub_pyclass(module = "nautilus_trader.common")]
pub struct PyMessageBus {
    trader_id: TraderId,
    instance_id: UUID4,
    name: String,
    has_backing: bool,
    serializer: Option<Py<PyAny>>,
    backing: Option<Py<PyAny>>,
    listeners: Vec<Py<PyAny>>,
    types_filter: Option<Py<PyAny>>,
    streaming_types: Vec<Py<PyAny>>,
    correlation_index: AHashMap<UUID4, Py<PyAny>>,
    sent_count: u64,
    req_count: u64,
    res_count: u64,
    pub_count: u64,
}

impl Debug for PyMessageBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(PyMessageBus))
            .field("trader_id", &self.trader_id)
            .field("name", &self.name)
            .finish()
    }
}

#[pymethods]
#[pyo3_stub_gen::derive::gen_stub_pymethods]
impl PyMessageBus {
    /// Creates a new `MessageBus` instance.
    ///
    /// This creates and registers the underlying Rust `MessageBus` as the
    /// thread-local bus, then wraps it for Python access.
    #[new]
    #[pyo3(signature = (trader_id, clock=None, instance_id=None, name=None, serializer=None, backing=None, config=None))]
    #[expect(clippy::too_many_arguments, clippy::needless_pass_by_value)]
    fn py_new(
        py: Python<'_>,
        trader_id: TraderId,
        clock: Option<Py<PyAny>>,
        instance_id: Option<UUID4>,
        name: Option<String>,
        serializer: Option<Py<PyAny>>,
        backing: Option<Py<PyAny>>,
        config: Option<Py<PyAny>>,
    ) -> PyResult<Self> {
        let _ = clock;
        let instance_id = instance_id.unwrap_or_default();
        let bus_name = name.clone();
        let has_backing = backing.is_some();

        let msgbus = MessageBus::new(trader_id, instance_id, bus_name, None);
        msgbus.register_message_bus();

        let types_filter = if let Some(ref cfg) = config {
            let tf = cfg.getattr(py, "types_filter")?;
            if tf.is_none(py) {
                None
            } else {
                // Convert to tuple for isinstance() checks
                let tuple = py
                    .import("builtins")?
                    .call_method1("tuple", (tf,))?
                    .unbind();
                Some(tuple)
            }
        } else {
            None
        };

        Ok(Self {
            trader_id,
            instance_id,
            name: name.unwrap_or_else(|| "MessageBus".to_owned()),
            has_backing,
            serializer,
            backing,
            listeners: Vec::new(),
            types_filter,
            streaming_types: Vec::new(),
            correlation_index: AHashMap::new(),
            sent_count: 0,
            req_count: 0,
            res_count: 0,
            pub_count: 0,
        })
    }

    /// Returns the trader ID associated with the message bus.
    #[getter]
    #[pyo3(name = "trader_id")]
    fn py_trader_id(&self) -> TraderId {
        self.trader_id
    }

    /// Returns the instance ID associated with the message bus.
    #[getter]
    #[pyo3(name = "instance_id")]
    fn py_instance_id(&self) -> UUID4 {
        self.instance_id
    }

    /// Returns the name of the message bus.
    #[getter]
    #[pyo3(name = "name")]
    fn py_name(&self) -> &str {
        &self.name
    }

    /// Returns whether the message bus has an external backing.
    #[getter]
    #[pyo3(name = "has_backing")]
    fn py_has_backing(&self) -> bool {
        self.has_backing
    }

    /// Returns the count of messages sent via point-to-point.
    #[getter]
    #[pyo3(name = "sent_count")]
    fn py_sent_count(&self) -> u64 {
        self.sent_count
    }

    /// Returns the count of requests made.
    #[getter]
    #[pyo3(name = "req_count")]
    fn py_req_count(&self) -> u64 {
        self.req_count
    }

    /// Returns the count of responses handled.
    #[getter]
    #[pyo3(name = "res_count")]
    fn py_res_count(&self) -> u64 {
        self.res_count
    }

    /// Returns the count of messages published.
    #[getter]
    #[pyo3(name = "pub_count")]
    fn py_pub_count(&self) -> u64 {
        self.pub_count
    }

    /// Returns all registered endpoint addresses.
    #[pyo3(name = "endpoints")]
    fn py_endpoints(&self) -> Vec<String> {
        let bus = get_message_bus();
        let bus_ref = bus.borrow();
        bus_ref.endpoints().into_iter().map(String::from).collect()
    }

    /// Returns all topics with active subscribers.
    #[pyo3(name = "topics")]
    fn py_topics(&self) -> Vec<String> {
        let bus = get_message_bus();
        let bus_ref = bus.borrow();
        let mut topics: Vec<String> = bus_ref.patterns().into_iter().map(String::from).collect();
        topics.sort();
        topics.dedup();
        topics
    }

    /// Returns subscriptions matching the given topic pattern.
    #[pyo3(name = "subscriptions")]
    #[pyo3(signature = (pattern=None))]
    fn py_subscriptions(&self, pattern: Option<&str>) -> PyResult<Vec<String>> {
        let filter = pattern.map(parse_pattern).transpose()?;

        let bus = get_message_bus();
        let bus_ref = bus.borrow();
        let subs: Vec<&Subscription> = bus_ref.subscriptions();

        Ok(subs
            .into_iter()
            .filter(|s| filter.is_none_or(|f| is_matching(s.pattern.as_bytes(), f.as_bytes())))
            .map(|s| {
                format!(
                    "Subscription(topic={}, handler={})",
                    s.pattern, s.handler_id
                )
            })
            .collect())
    }

    /// Returns whether there are subscribers for the given topic pattern.
    #[pyo3(name = "has_subscribers")]
    #[pyo3(signature = (pattern=None))]
    fn py_has_subscribers(&self, pattern: Option<&str>) -> PyResult<bool> {
        let filter = pattern.map(parse_pattern).transpose()?;

        let bus = get_message_bus();
        let bus_ref = bus.borrow();

        Ok(match filter {
            Some(filter) => bus_ref
                .subscriptions()
                .iter()
                .any(|s| is_matching(s.pattern.as_bytes(), filter.as_bytes())),
            None => !bus_ref.subscriptions().is_empty(),
        })
    }

    /// Returns whether the given topic and handler is subscribed.
    #[pyo3(name = "is_subscribed")]
    fn py_is_subscribed(&self, py: Python<'_>, topic: &str, handler: Py<PyAny>) -> PyResult<bool> {
        let pattern = parse_pattern(topic)?;
        let handler = make_handler(py, handler)?;
        let sub = Subscription::new(pattern, handler, None);
        Ok(get_message_bus().borrow().subscriptions.contains(&sub))
    }

    /// Returns whether the given request ID is pending a response.
    #[pyo3(name = "is_pending_request")]
    fn py_is_pending_request(&self, request_id: UUID4) -> bool {
        self.correlation_index.contains_key(&request_id)
    }

    /// Returns whether the given type is registered for streaming.
    #[pyo3(name = "is_streaming_type")]
    #[expect(clippy::needless_pass_by_value)]
    fn py_is_streaming_type(&self, py: Python<'_>, cls: Py<PyAny>) -> bool {
        let cls_ref = cls.bind(py);
        self.streaming_types.iter().any(|t| t.bind(py).is(cls_ref))
    }

    /// Returns all types registered for streaming.
    #[pyo3(name = "streaming_types")]
    fn py_streaming_types(&self, py: Python<'_>) -> Vec<Py<PyAny>> {
        self.streaming_types
            .iter()
            .map(|t| t.clone_ref(py))
            .collect()
    }

    /// Registers a handler at the given endpoint address.
    #[pyo3(name = "register")]
    fn py_register(&self, py: Python<'_>, endpoint: &str, handler: Py<PyAny>) -> PyResult<()> {
        let endpoint = parse_endpoint(endpoint)?;
        let handler = make_handler(py, handler)?;
        msgbus_api::register_any(endpoint, handler);
        Ok(())
    }

    /// Deregisters the handler from the given endpoint address.
    #[pyo3(name = "deregister")]
    #[pyo3(signature = (endpoint, handler=None))]
    #[expect(clippy::needless_pass_by_value)]
    fn py_deregister(&self, endpoint: &str, handler: Option<Py<PyAny>>) -> PyResult<()> {
        let _ = handler;
        let endpoint = parse_endpoint(endpoint)?;
        msgbus_api::deregister_any(endpoint);
        Ok(())
    }

    /// Sends a message to the given endpoint address.
    #[pyo3(name = "send")]
    fn py_send(&mut self, endpoint: &str, msg: Py<PyAny>) -> PyResult<()> {
        let endpoint = parse_endpoint(endpoint)?;
        let py_msg = PyMessage(msg);
        msgbus_api::send_any(endpoint, &py_msg);
        self.sent_count += 1;
        Ok(())
    }

    /// Sends a request to the given endpoint with correlation tracking.
    #[pyo3(name = "request")]
    fn py_request(&mut self, py: Python<'_>, endpoint: &str, request: Py<PyAny>) -> PyResult<()> {
        let endpoint = parse_endpoint(endpoint)?;
        let request_ref = request.bind(py);

        let request_id: UUID4 = request_ref.getattr("id")?.extract()?;
        let callback = request_ref.getattr("callback")?;

        if self.correlation_index.contains_key(&request_id) {
            log::error!(
                "Cannot handle request: duplicate ID {request_id} found in correlation index"
            );
            return Ok(());
        }

        if !callback.is_none() {
            self.correlation_index.insert(request_id, callback.unbind());
        }

        let py_msg = PyMessage(request);
        msgbus_api::send_any(endpoint, &py_msg);
        self.req_count += 1;

        Ok(())
    }

    /// Handles a response by invoking the correlated callback.
    #[pyo3(name = "response")]
    #[expect(clippy::needless_pass_by_value)]
    fn py_response(&mut self, py: Python<'_>, response: Py<PyAny>) -> PyResult<()> {
        let correlation_id: UUID4 = response.getattr(py, "correlation_id")?.extract(py)?;

        if let Some(callback) = self.correlation_index.remove(&correlation_id) {
            callback.call1(py, (&response,))?;
        } else {
            log::debug!("No callback for correlation_id {correlation_id}");
        }

        self.res_count += 1;
        Ok(())
    }

    /// Subscribes to the given topic with the given handler.
    #[pyo3(name = "subscribe")]
    #[pyo3(signature = (topic, handler, priority=0))]
    fn py_subscribe(
        &self,
        py: Python<'_>,
        topic: &str,
        handler: Py<PyAny>,
        priority: u32,
    ) -> PyResult<()> {
        let pattern = parse_pattern(topic)?;
        let handler = make_handler(py, handler)?;
        msgbus_api::subscribe_any(pattern, handler, Some(priority));
        Ok(())
    }

    /// Unsubscribes the given handler from the given topic.
    #[pyo3(name = "unsubscribe")]
    fn py_unsubscribe(&self, py: Python<'_>, topic: &str, handler: Py<PyAny>) -> PyResult<()> {
        let pattern = parse_pattern(topic)?;
        let handler = make_handler(py, handler)?;
        msgbus_api::unsubscribe_any(pattern, &handler);
        Ok(())
    }

    /// Publishes a message for the given topic.
    #[pyo3(name = "publish")]
    #[pyo3(signature = (topic, msg, external_pub=true))]
    #[expect(clippy::needless_pass_by_value)]
    fn py_publish(
        &mut self,
        py: Python<'_>,
        topic: &str,
        msg: Py<PyAny>,
        external_pub: bool,
    ) -> PyResult<()> {
        let topic_mstr = MStr::<Topic>::topic(topic).map_err(to_pyruntime_err)?;

        let py_msg = PyMessage(msg.clone_ref(py));
        msgbus_api::publish_any(topic_mstr, &py_msg);

        if external_pub {
            self.publish_external(py, topic, &msg)?;
        }

        self.pub_count += 1;
        Ok(())
    }

    /// Disposes of the message bus, clearing all state.
    #[pyo3(name = "dispose")]
    fn py_dispose(&mut self, py: Python<'_>) -> PyResult<()> {
        log::debug!("Closing message bus");

        get_message_bus().borrow_mut().dispose();

        self.correlation_index.clear();
        self.listeners.clear();
        self.streaming_types.clear();

        if let Some(ref backing) = self.backing {
            let db = backing.bind(py);
            if !db.call_method0("is_closed")?.extract::<bool>()? {
                db.call_method0("close")?;
            }
        }

        log::info!("Closed message bus");
        Ok(())
    }

    /// Registers a type for external-to-internal message streaming.
    #[pyo3(name = "add_streaming_type")]
    fn py_add_streaming_type(&mut self, cls: Py<PyAny>) {
        self.streaming_types.push(cls);
    }

    /// Adds a listener to the message bus.
    #[pyo3(name = "add_listener")]
    fn py_add_listener(&mut self, listener: Py<PyAny>) {
        self.listeners.push(listener);
    }
}

impl PyMessageBus {
    fn publish_external(&self, py: Python<'_>, topic: &str, msg: &Py<PyAny>) -> PyResult<()> {
        if let Some(ref filter) = self.types_filter {
            let is_excluded = py
                .import("builtins")?
                .call_method1("isinstance", (msg, filter))?
                .extract::<bool>()?;

            if is_excluded {
                return Ok(());
            }
        }

        // Serialize: raw bytes pass through, other types need a serializer
        let msg_ref = msg.bind(py);
        let payload: Py<PyAny> = if msg_ref.is_instance_of::<pyo3::types::PyBytes>() {
            msg.clone_ref(py)
        } else if let Some(ref serializer) = self.serializer {
            serializer.call_method1(py, "serialize", (msg,))?
        } else {
            return Ok(());
        };

        if let Some(ref backing) = self.backing {
            let db = backing.bind(py);
            if !db.call_method0("is_closed")?.extract::<bool>()? {
                db.call_method1("publish", (topic, &payload))?;
            }
        }

        for listener in &self.listeners {
            let l = listener.bind(py);
            if l.call_method0("is_closed")?.extract::<bool>()? {
                continue;
            }
            l.call_method1("publish", (topic, &payload))?;
        }

        Ok(())
    }
}

fn parse_endpoint(endpoint: &str) -> PyResult<MStr<Endpoint>> {
    MStr::<Endpoint>::endpoint(endpoint).map_err(to_pyvalue_err)
}

fn parse_pattern(pattern: &str) -> PyResult<MStr<Pattern>> {
    MStr::<Pattern>::pattern_checked(pattern).map_err(to_pyvalue_err)
}

#[cfg(test)]
mod tests {
    use std::{any::Any, ffi::CString};

    use nautilus_model::identifiers::{ClientOrderId, VenueOrderId};
    use pyo3::{exceptions::PyValueError, ffi::c_str};
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn test_message_bus_factory_registry_compatibility_constructors() {
        let registry = MessageBusFactoryRegistry::new();
        let default_registry = MessageBusFactoryRegistry::default();

        assert_eq!(format!("{registry:?}"), format!("{default_registry:?}"));
        assert!(format!("{registry:?}").contains("message bus factory"));
    }

    #[rstest]
    fn message_bus_config_py_new_maps_validate_error_to_value_error() {
        pyo3::Python::initialize();
        Python::attach(|py| {
            let err = MessageBusConfig::py_new(
                Some(SerializationEncoding::Json),
                None,
                Some(SerializationEncoding::Capnp),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap_err();

            assert!(err.is_instance_of::<PyValueError>(py));
            assert_eq!(
                err.value(py).to_string(),
                format!(
                    "MessageBusConfig.encoding_builtin has unsupported value: {} is not supported by AccountState, OrderEventAny, PositionEvent, PortfolioSnapshot",
                    SerializationEncoding::Capnp
                )
            );
        });
    }

    #[rstest]
    fn test_py_message_downcast() {
        pyo3::Python::initialize();
        Python::attach(|py| {
            let py_obj = py.eval(c_str!("42"), None, None).unwrap();
            let msg = PyMessage(py_obj.unbind());

            let any_ref: &dyn Any = &msg;
            let downcasted = any_ref.downcast_ref::<PyMessage>();
            assert!(downcasted.is_some());

            let inner = &downcasted.unwrap().0;
            let value: i64 = inner.extract(py).unwrap();
            assert_eq!(value, 42);
        });
    }

    #[rstest]
    fn test_py_callable_handler_id_stability() {
        pyo3::Python::initialize();
        Python::attach(|py| {
            let callable = py.eval(c_str!("lambda x: x"), None, None).unwrap().unbind();

            let handler1 = PyCallableHandler::new(py, callable.clone_ref(py)).unwrap();
            let handler2 = PyCallableHandler::new(py, callable).unwrap();

            assert_eq!(handler1.id(), handler2.id());
        });
    }

    #[rstest]
    fn test_py_callable_handler_dispatch() {
        pyo3::Python::initialize();
        Python::attach(|py| {
            let main = py.import("__main__").unwrap();
            let globals = main.dict();
            py.run(
                c_str!("results = []\ndef handler(x): results.append(x)"),
                Some(&globals),
                None,
            )
            .unwrap();

            let handler_fn = globals.get_item("handler").unwrap().unwrap().unbind();
            let handler = PyCallableHandler::new(py, handler_fn).unwrap();

            let py_obj = py.eval(c_str!("'hello'"), None, None).unwrap();
            let msg = PyMessage(py_obj.unbind());

            let any_ref: &dyn Any = &msg;
            handler.handle(any_ref);

            let results = globals.get_item("results").unwrap().unwrap();
            let len: usize = results.len().unwrap();
            assert_eq!(len, 1);
        });
    }

    fn rust_order_status_report(raw_order_status: &str) -> OrderStatusReport {
        use nautilus_core::{UUID4, UnixNanos};
        use nautilus_model::{
            enums::{OrderSide, OrderStatus, OrderType, TimeInForce},
            identifiers::{AccountId, ClientOrderId, InstrumentId, VenueOrderId},
            types::Quantity,
        };
        use rust_decimal::Decimal;

        let mut report = OrderStatusReport::new(
            AccountId::from("IB-DU1234567"),
            InstrumentId::from("BTC/USD.PAXOS"),
            Some(ClientOrderId::from("O-C2-8-P1-001")),
            VenueOrderId::from("B-C2-8-P1-001"),
            Some(OrderSide::Buy),
            OrderType::Limit,
            TimeInForce::Gtc,
            OrderStatus::Submitted,
            Quantity::from("100"),
            Quantity::from("0"),
            UnixNanos::from(1_700_000_000_000_000_001),
            UnixNanos::from(1_700_000_000_000_000_002),
            UnixNanos::from(1_700_000_000_000_000_003),
            Some(UUID4::from("00000000-0000-4000-8000-0000000000c2")),
        )
        .with_raw_order_status(raw_order_status.to_string());
        report.avg_px = Some(Decimal::ZERO);
        report
    }

    fn capturing_python_handler<'py>(
        py: Python<'py>,
        source: &str,
    ) -> (PyCallableHandler, Bound<'py, pyo3::types::PyDict>) {
        let main = py.import("__main__").unwrap();
        let globals = main.dict();
        let code = CString::new(source).unwrap();
        py.run(code.as_c_str(), Some(&globals), None).unwrap();
        let handler_fn = globals.get_item("handler").unwrap().unwrap().unbind();
        let handler = PyCallableHandler::new(py, handler_fn).unwrap();
        (handler, globals)
    }

    #[rstest]
    fn test_py_callable_handler_converts_rust_order_status_report() {
        pyo3::Python::initialize();
        Python::attach(|py| {
            let (handler, globals) =
                capturing_python_handler(py, "results = []\ndef handler(x): results.append(x)");
            let report = rust_order_status_report("PendingSubmit");
            handler.handle(&report);

            let results = globals.get_item("results").unwrap().unwrap();
            assert_eq!(results.len().unwrap(), 1);
            let received = results.get_item(0).unwrap();
            assert_eq!(
                received.get_type().name().unwrap().to_string_lossy(),
                "OrderStatusReport"
            );
            let extracted: OrderStatusReport = received.extract().unwrap();
            assert_eq!(extracted, report);
            assert_eq!(extracted.raw_order_status.as_deref(), Some("PendingSubmit"));
            assert_eq!(extracted.account_id.to_string(), "IB-DU1234567");
            assert_eq!(
                extracted.client_order_id.unwrap().to_string(),
                "O-C2-8-P1-001"
            );
            assert_eq!(extracted.venue_order_id.to_string(), "B-C2-8-P1-001");
            assert_eq!(extracted.instrument_id.to_string(), "BTC/USD.PAXOS");
        });
    }

    #[rstest]
    fn test_publish_any_delivers_order_status_report_to_python_subscriber() {
        pyo3::Python::initialize();
        Python::attach(|py| {
            get_message_bus().borrow_mut().dispose();

            let (handler, globals) =
                capturing_python_handler(py, "results = []\ndef handler(x): results.append(x)");
            let shareable = TypedHandler(Rc::new(handler) as Rc<dyn Handler<dyn Any>>);
            let topic =
                crate::msgbus::MessagingSwitchboard::reconciliation_raw_order_status_report_topic();
            let pattern: MStr<Pattern> = topic.into();
            msgbus_api::subscribe_any(pattern, shareable, None);

            let pending = rust_order_status_report("PendingSubmit");
            let unknown = rust_order_status_report("SomeFutureIbkrStatus");
            msgbus_api::publish_any(topic, &pending);
            msgbus_api::publish_any(topic, &unknown);
            msgbus_api::publish_any(topic, &unknown);

            let results = globals.get_item("results").unwrap().unwrap();
            assert_eq!(
                results.len().unwrap(),
                3,
                "one Rust publish must yield one Python invocation; duplicates are not deduped"
            );

            let first: OrderStatusReport = results.get_item(0).unwrap().extract().unwrap();
            let second: OrderStatusReport = results.get_item(1).unwrap().extract().unwrap();
            let third: OrderStatusReport = results.get_item(2).unwrap().extract().unwrap();
            assert_eq!(first, pending);
            assert_eq!(second, unknown);
            assert_eq!(third, unknown);
            assert_eq!(first.raw_order_status.as_deref(), Some("PendingSubmit"));
            assert_eq!(
                second.raw_order_status.as_deref(),
                Some("SomeFutureIbkrStatus")
            );
            assert_eq!(first.quantity, pending.quantity);
            assert_eq!(first.filled_qty, pending.filled_qty);
            assert_eq!(first.avg_px, pending.avg_px);
            assert_eq!(first.report_id, pending.report_id);
            assert_eq!(first.ts_accepted, pending.ts_accepted);
            assert_eq!(first.ts_last, pending.ts_last);
            assert_eq!(first.order_status, pending.order_status);

            get_message_bus().borrow_mut().dispose();
        });
    }

    #[rstest]
    fn test_python_callback_exception_is_contained_for_order_status_report() {
        pyo3::Python::initialize();
        Python::attach(|py| {
            get_message_bus().borrow_mut().dispose();

            let (handler, globals) = capturing_python_handler(
                py,
                "results = []\ndef handler(x):\n    results.append(x)\n    raise RuntimeError('boom')",
            );
            let shareable = TypedHandler(Rc::new(handler) as Rc<dyn Handler<dyn Any>>);
            let topic =
                crate::msgbus::MessagingSwitchboard::reconciliation_raw_order_status_report_topic();
            msgbus_api::subscribe_any(topic.into(), shareable, None);

            let first = rust_order_status_report("PendingSubmit");
            let second = rust_order_status_report("Submitted");
            msgbus_api::publish_any(topic, &first);
            msgbus_api::publish_any(topic, &second);

            let results = globals.get_item("results").unwrap().unwrap();
            assert_eq!(
                results.len().unwrap(),
                2,
                "callback exceptions must remain contained so later deliveries still occur"
            );
            let received_first: OrderStatusReport = results.get_item(0).unwrap().extract().unwrap();
            let received_second: OrderStatusReport =
                results.get_item(1).unwrap().extract().unwrap();
            assert_eq!(received_first, first);
            assert_eq!(received_second, second);

            get_message_bus().borrow_mut().dispose();
        });
    }

    #[rstest]
    fn test_py_callable_handler_still_rejects_unbridged_rust_types() {
        pyo3::Python::initialize();
        Python::attach(|py| {
            let (handler, globals) =
                capturing_python_handler(py, "results = []\ndef handler(x): results.append(x)");
            handler.handle(&42_i32);
            let results = globals.get_item("results").unwrap().unwrap();
            assert_eq!(results.len().unwrap(), 0);
        });
    }

    #[rstest]
    fn test_rust_publish_reaches_existing_c2_8_handler() {
        pyo3::Python::initialize();
        Python::attach(|py| {
            get_message_bus().borrow_mut().dispose();

            let sys = py.import("sys").unwrap();
            let path = sys.getattr("path").unwrap();
            path.call_method1("insert", (0, "/home/agent/projects/nautilus-extensions"))
                .unwrap();

            let setup = CString::new(
                r#"
import tempfile
from decimal import Decimal as D
from pathlib import Path

from nautilus_extensions.account_state import AccountStateFacts
from nautilus_extensions.admission_coordinator import AdmissionCoordinator
from nautilus_extensions.admission_store import AdmissionStore, STATE_SUBMITTED
from nautilus_extensions.broker_state_translator import UNKNOWN_RAW_BROKER_STATUS
from nautilus_extensions.order_status_delivery import (
    RECONCILIATION_RAW_ORDER_STATUS_REPORT_TOPIC,
    NautilusOrderStatusReportHandler,
)
from nautilus_extensions.risk import (
    STANDARD_DRAWDOWN_LADDER,
    DecisionStatus,
    RiskEffect,
    RiskPolicy,
    Side,
    TradeIntent,
)

ACCOUNT = "IB-DU1234567"
ORDER_ID = "o-1"
CLIENT = "O-C2-8-001"
INST = "BTC/USD.PAXOS"
TOPIC = RECONCILIATION_RAW_ORDER_STATUS_REPORT_TOPIC
tmp = tempfile.TemporaryDirectory()
path = str(Path(tmp.name) / "ledger.sqlite")
store = AdmissionStore(path, ACCOUNT, busy_timeout_ms=1000)
coord = AdmissionCoordinator(store)
policy = RiskPolicy(
    policy_id="pol-1", policy_version="1",
    max_loss_per_trade=D("10000"), safety_reserve=D("0"),
    max_instrument_exposure=None, max_portfolio_gross_exposure=None,
    max_daily_realised_loss=None,
    drawdown_ladder=STANDARD_DRAWDOWN_LADDER)
intent = TradeIntent(
    intent_id="i-1", instrument_id=INST, side=Side.LONG,
    quantity=D("100"), reference_price=D("100"),
    risk_effect=RiskEffect.INCREASING, strategy_id="strat-a",
    strategy_version="1", stop_price=D("99"), per_unit_max_loss=None)
facts = AccountStateFacts(
    snapshot_id="snap-1", nav=D("1000000"),
    cash_settled=D("150000"), high_water_nav=D("1000000"),
    daily_realised_loss=D("0"), risk_state=None,
    positions=(), pending_reservations=())
result = coord.admit(intent, facts, 1, policy)
assert result.decision.status in (DecisionStatus.APPROVE, DecisionStatus.RESIZE)
coord.bind_order(ACCOUNT, ORDER_ID, "i-1", client_order_id=CLIENT)
coord.begin_submission(ACCOUNT, ORDER_ID, "s-1")
results = []
handler = NautilusOrderStatusReportHandler(coord, result_sink=results.append)
"#,
            )
            .unwrap();

            let main = py.import("__main__").unwrap();
            let globals = main.dict();
            py.run(setup.as_c_str(), Some(&globals), None).unwrap();

            let handler_obj = globals.get_item("handler").unwrap().unwrap().unbind();
            let topic_str: String = globals
                .get_item("TOPIC")
                .unwrap()
                .unwrap()
                .extract()
                .unwrap();
            assert_eq!(topic_str, "reconciliation.raw.OrderStatusReport");

            let bus = PyMessageBus::py_new(
                py,
                TraderId::from("TRADER-001"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            bus.py_subscribe(py, &topic_str, handler_obj, 0).unwrap();

            let pending = {
                let mut report = rust_order_status_report("PendingSubmit");
                report.client_order_id = Some(ClientOrderId::from("O-C2-8-001"));
                report.venue_order_id = VenueOrderId::from("B-C2-8-001");
                report
            };
            let unknown = {
                let mut report = rust_order_status_report("SomeFutureIbkrStatus");
                report.client_order_id = Some(ClientOrderId::from("O-C2-8-001"));
                report.venue_order_id = VenueOrderId::from("B-C2-8-001");
                report
            };
            let topic =
                crate::msgbus::MessagingSwitchboard::reconciliation_raw_order_status_report_topic();
            msgbus_api::publish_any(topic, &pending);
            msgbus_api::publish_any(topic, &unknown);

            let results = globals.get_item("results").unwrap().unwrap();
            assert_eq!(results.len().unwrap(), 2);

            let pending_result = results.get_item(0).unwrap();
            assert!(
                pending_result
                    .getattr("accepted")
                    .unwrap()
                    .extract::<bool>()
                    .unwrap()
            );
            assert_eq!(
                pending_result
                    .getattr("order_id")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "o-1"
            );
            assert_eq!(
                pending_result
                    .getattr("order_state")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "SUBMITTED"
            );
            assert!(
                pending_result
                    .getattr("mutation_occurred")
                    .unwrap()
                    .extract::<bool>()
                    .unwrap()
            );

            let unknown_result = results.get_item(1).unwrap();
            assert!(
                !unknown_result
                    .getattr("accepted")
                    .unwrap()
                    .extract::<bool>()
                    .unwrap()
            );
            assert_eq!(
                unknown_result
                    .getattr("reason_code")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "UNKNOWN_RAW_BROKER_STATUS"
            );
            let unknown_message: String = unknown_result
                .getattr("reason_message")
                .unwrap()
                .extract()
                .unwrap();
            assert!(
                unknown_message.contains("SomeFutureIbkrStatus"),
                "unknown raw status must survive the bridge: {unknown_message}"
            );
            assert!(
                !unknown_result
                    .getattr("mutation_occurred")
                    .unwrap()
                    .extract::<bool>()
                    .unwrap()
            );

            get_message_bus().borrow_mut().dispose();
        });
    }
}
