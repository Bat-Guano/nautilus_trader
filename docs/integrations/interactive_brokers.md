# Interactive Brokers

Interactive Brokers (IB) provides market access across equities, options, futures, currencies,
bonds, funds, and other asset classes. The NautilusTrader adapter connects to Trader Workstation
(TWS) or IB Gateway through the [TWS API](https://ibkrcampus.com/campus/ibkr-api-page/twsapi-doc/).

The adapter provides live market data, execution, historical data, instrument loading, and optional
Dockerized IB Gateway management through the same Rust implementation and Python bindings.

## Installation

Install NautilusTrader using the [installation guide](../getting_started/installation.md). The
Interactive Brokers adapter and Docker gateway support are included in the Python package; no
adapter-specific extra is required.

## Examples

- [Python examples](https://github.com/nautechsystems/nautilus_trader/tree/develop/examples/live/interactive_brokers/)
- [Rust examples](https://github.com/nautechsystems/nautilus_trader/tree/develop/crates/adapters/interactive_brokers/examples/)

## Getting started

Run either TWS or IB Gateway before starting a client, and configure that application to accept
socket API connections.

IB uses different default ports for each application and trading mode:

| Application | Paper trading | Live trading |
| ----------- | ------------: | -----------: |
| TWS         |        `7497` |       `7496` |
| IB Gateway  |        `4002` |       `4001` |

The adapter defaults to `127.0.0.1:4002`, which matches a local paper-trading IB Gateway. Set the
port explicitly when using TWS or a live account.

### Connect to TWS or IB Gateway

Import the public configuration types from `nautilus_trader.adapters.interactive_brokers`:

```python
from nautilus_trader.adapters.interactive_brokers import InteractiveBrokersDataClientConfig
from nautilus_trader.adapters.interactive_brokers import InteractiveBrokersExecutionClientConfig
from nautilus_trader.adapters.interactive_brokers import MarketDataType


data_config = InteractiveBrokersDataClientConfig(
    host="127.0.0.1",
    port=7497,
    client_id=101,
    market_data_type=MarketDataType.DELAYED,
)

exec_config = InteractiveBrokersExecutionClientConfig(
    host="127.0.0.1",
    port=7497,
    client_id=101,
    account_id="DU123456",
)
```

Use a distinct client ID for each process connected to the same TWS or IB Gateway session. An
execution client ID cannot be a multiple of `1000` because the adapter partitions order IDs by
`client_id % 1000`.

The current [TWS example](https://github.com/nautechsystems/nautilus_trader/blob/develop/examples/live/interactive_brokers/connect_with_tws.py)
and [Dockerized gateway example](https://github.com/nautechsystems/nautilus_trader/blob/develop/examples/live/interactive_brokers/connect_with_dockerized_gateway.py)
show how to add these configs and their factories to a `LiveNode`.

### Use a Dockerized IB Gateway

The adapter can manage the
[gnzsnz IB Gateway container](https://github.com/gnzsnz/ib-gateway-docker). Supply credentials in
the config or through `TWS_USERNAME` and `TWS_PASSWORD`:

```python
from nautilus_trader.adapters.interactive_brokers import DockerizedIBGateway
from nautilus_trader.adapters.interactive_brokers import DockerizedIBGatewayConfig
from nautilus_trader.adapters.interactive_brokers import TradingMode


gateway = DockerizedIBGateway(
    DockerizedIBGatewayConfig(
        trading_mode=TradingMode.PAPER,
        read_only_api=True,
    ),
)
gateway.start_blocking()

print(gateway.host)
print(gateway.port)
```

Start `DockerizedIBGateway` separately, then pass its `host` and `port` to the data and execution
configs. Passing a non-`None` `dockerized_gateway` argument to either client config raises
`ValueError` because Python does not own the container lifecycle.

Set `read_only_api=False` only when the gateway must submit orders. The default container is
`ghcr.io/gnzsnz/ib-gateway:stable`; `vnc_port` accepts ports from `5900` through `5999` when remote
desktop access is required.

## Components

The public Python module exports these main components:

- `InteractiveBrokersDataClientFactory`: creates live market data clients.
- `InteractiveBrokersExecutionClientFactory`: creates live execution clients.
- `InteractiveBrokersInstrumentProvider`: resolves IB contracts and Nautilus instruments.
- `HistoricalInteractiveBrokersClient`: requests historical instruments, bars, and ticks.
- `DockerizedIBGateway`: manages a containerized IB Gateway.

## Symbology and instruments

`InteractiveBrokersInstrumentProviderConfig` supports two symbology methods:

| Method                       | Purpose                                       | Example    |
| ---------------------------- | --------------------------------------------- | ---------- |
| `SymbologyMethod.SIMPLIFIED` | Uses shorter, readable symbols.               | `EUR/USD`  |
| `SymbologyMethod.RAW`        | Preserves the IB security type in the symbol. | `AAPL=STK` |

The default is `SIMPLIFIED`. Use `RAW` when the security type must remain explicit in the
instrument ID, as in `AAPL=STK.SMART`.

Configure instruments by Nautilus instrument ID or by IB contract dictionaries:

```python
from nautilus_trader.adapters.interactive_brokers import InteractiveBrokersInstrumentProviderConfig
from nautilus_trader.adapters.interactive_brokers import SymbologyMethod
from nautilus_trader.model import InstrumentId


provider_config = InteractiveBrokersInstrumentProviderConfig(
    symbology_method=SymbologyMethod.RAW,
    load_ids={InstrumentId.from_str("AAPL=STK.SMART")},
    load_contracts=[
        {
            "symbol": "MSFT",
            "secType": "STK",
            "exchange": "SMART",
            "currency": "USD",
        },
    ],
)
```

The same provider config can be passed to both data and execution client configs. This keeps
contract resolution and instrument IDs consistent across both clients.

### Instrument provider options

| Option                          | Default      | Purpose                                                 |
| ------------------------------- | ------------ | ------------------------------------------------------- |
| `symbology_method`              | `SIMPLIFIED` | Select simplified or raw instrument symbols.            |
| `load_ids`                      | Empty        | Load Nautilus instrument IDs at startup.                |
| `load_contracts`                | Empty        | Load IB contract dictionaries at startup.               |
| `min_expiry_days`               | `None`       | Set the minimum expiry for chain loading.               |
| `max_expiry_days`               | `None`       | Set the maximum expiry for chain loading.               |
| `build_options_chain`           | `None`       | Control full option chain construction.                 |
| `build_futures_chain`           | `None`       | Control full futures chain construction.                |
| `cache_validity_days`           | `None`       | Set the lifetime of cached instrument data.             |
| `convert_exchange_to_mic_venue` | `False`      | Convert IB exchange codes to MIC venues.                |
| `symbol_to_mic_venue`           | Empty        | Override MIC venues for selected symbols.               |
| `filter_sec_types`              | Empty        | Exclude selected IB security types.                     |
| `filter_callable`               | `None`       | Apply a Python callable by fully qualified import path. |
| `cache_path`                    | `None`       | Persist the instrument cache at the selected path.      |

### Derivative chains and spreads

Set chain flags on a contract dictionary to use that contract as the underlying or chain seed.
The provider-level `min_expiry_days` and `max_expiry_days` values limit the contracts loaded:

```python
from nautilus_trader.adapters.interactive_brokers import InteractiveBrokersInstrumentProviderConfig


provider_config = InteractiveBrokersInstrumentProviderConfig(
    load_contracts=[
        {
            "symbol": "SPY",
            "secType": "STK",
            "exchange": "SMART",
            "currency": "USD",
            "build_options_chain": True,
        },
        {
            "symbol": "ES",
            "secType": "CONTFUT",
            "exchange": "CME",
            "currency": "USD",
            "build_futures_chain": True,
        },
    ],
    min_expiry_days=7,
    max_expiry_days=60,
)
```

When `CONTFUT` has a chain flag, the adapter qualifies it and loads the matching dated futures or
futures options. Without a chain flag, it represents IB's continuous future, which IB limits to
historical data. It cannot provide live market data or accept orders. See the
[IB continuous futures documentation](https://www.interactivebrokers.com/docs/general/contracts/futures/continuous-futures).

The adapter also resolves IB `BAG` contracts from Nautilus spread instrument IDs. Request a spread
before subscribing to it or trading it:

```python
from nautilus_trader.model import InstrumentId


spread_id = InstrumentId.from_str("(1)SPY C400_((1))SPY C410.SMART")
self.request_instrument(spread_id)
```

Single parentheses mark a positive leg ratio; double parentheses mark a negative ratio. All legs
must use the same venue. IB requires a contract ID, ratio, action, and exchange for each combo leg;
see [Spreads in the TWS API](https://www.interactivebrokers.com/docs/general/contracts/spread-contracts/twsapi-spreads/spreads-in-the-tws-api).

## Historical data

`HistoricalInteractiveBrokersClient` connects with an instrument provider and data client config.
Its async Python methods support:

- `request_instruments` for contract and instrument discovery.
- `request_bars` for one or more bar specifications.
- `request_ticks` for historical trade or bid-ask ticks.

For `CONTFUT` bar requests, the client omits `end_date_time` because IB rejects an explicit end
date. It requests only the first duration segment, anchored to the current time, so returned bars
may fall outside the requested start and end range.

IB controls historical availability, pacing, bar sizes, durations, and regular-trading-hours
filtering. Check the
[official historical bars](https://ibkrcampus.com/campus/ibkr-api-page/twsapi-doc/#historical-bars)
and [historical time and sales](https://ibkrcampus.com/campus/ibkr-api-page/twsapi-doc/#historical-time-sales)
documentation before selecting a request range.

## Order routing and IB attributes

Pass `params={"exchange": "..."}` when submitting an order, submitting an order list, or modifying
an order to override the cached contract exchange for that command. An empty or omitted value keeps
the cached exchange:

```python
self.submit_order(order, params={"exchange": "IEX"})
```

Pass IB-specific order attributes as a tag prefixed with `IBOrderTags:` and followed by a JSON
object. The adapter overlays recognized IB order fields and supports price, time, margin, execution,
volume, and percent-change conditions:

```python
import json


ib_attributes = {
    "ocaGroup": "MY_OCA_GROUP",
    "ocaType": 1,
    "conditionsCancelOrder": False,
    "conditions": [
        {
            "type": "price",
            "conId": 265598,
            "exchange": "SMART",
            "isMore": True,
            "price": 250.0,
            "triggerMethod": 0,
        },
    ],
}
tags = [f"IBOrderTags:{json.dumps(ib_attributes)}"]
```

Pass `tags` to the order factory. OCA type `1` cancels the remaining orders with overfill
protection; types `2` and `3` proportionally reduce the remaining orders with and without that
protection. See the [IB order reference](https://www.interactivebrokers.com/docs/tws-api/ref/order-class-reference/introduction)
for the supported order attributes.

## Configuration

### Data client

| Option                           | Default       | Purpose                                               |
| -------------------------------- | ------------- | ----------------------------------------------------- |
| `host`                           | `127.0.0.1`   | TWS or IB Gateway host.                               |
| `port`                           | `4002`        | TWS or IB Gateway socket port.                        |
| `client_id`                      | `1`           | IB API client ID.                                     |
| `use_regular_trading_hours`      | `True`        | Restrict requests to regular trading hours.           |
| `market_data_type`               | `REALTIME`    | Select real-time, frozen, delayed, or delayed frozen. |
| `ignore_quote_tick_size_updates` | `False`       | Ignore quote updates that change size only.           |
| `connection_timeout`             | `300` seconds | Set the socket connection timeout.                    |
| `request_timeout`                | `60` seconds  | Set the IB API request timeout.                       |
| `handle_revised_bars`            | `False`       | Process revised real-time bars.                       |
| `batch_quotes`                   | `True`        | Use `reqMktData` instead of tick-by-tick quotes.      |
| `instrument_provider`            | Default       | Configure contract and instrument loading.            |

### Execution client

| Option                                       | Default       | Purpose                                         |
| -------------------------------------------- | ------------- | ----------------------------------------------- |
| `host`                                       | `127.0.0.1`   | TWS or IB Gateway host.                         |
| `port`                                       | `4002`        | TWS or IB Gateway socket port.                  |
| `client_id`                                  | `1`           | IB API client ID.                               |
| `account_id`                                 | `None`        | Select the IB account.                          |
| `connection_timeout`                         | `300` seconds | Set the socket connection timeout.              |
| `request_timeout`                            | `60` seconds  | Set the IB API request timeout.                 |
| `fetch_all_open_orders`                      | `False`       | Request all open orders visible to the session. |
| `track_option_exercise_from_position_update` | `False`       | Infer option exercise from position updates.    |
| `instrument_provider`                        | Default       | Configure contract and instrument loading.      |

### Completed-order reconciliation

`generate_order_status_reports` builds one reconciliation snapshot from two broker sources:

| Source           | IB request           | Covers                                     |
| ---------------- | -------------------- | ------------------------------------------ |
| Open orders      | `reqAllOpenOrders`   | Orders still working at the venue.         |
| Completed orders | `reqCompletedOrders` | Orders that have left the open-order book. |

Open orders alone cannot prove terminal cancellation. Once IBKR cancels or completes an order it
disappears from the open-order response, so absence from that response is not evidence of a terminal
state. The adapter therefore also requests completed orders (`api_only = true`, i.e. orders placed by
this API client) and converts each record with `parse_completed_order_to_report`, producing the same
`OrderStatusReport` type used by the existing report publication path. No additional reconciliation
ingress is introduced.

Every identity in a completed-order report must come from broker evidence:

- `venue_order_id` is derived **only** from a strictly positive broker `perm_id` (`PERM-{perm_id}`). A
  completed record whose `perm_id` is zero or negative is rejected rather than assigned a fabricated venue
  identity: zero means IBKR has not acknowledged the order yet, and a non-positive value (for example a
  `-1` sentinel) is not valid broker venue identity. A non-positive `perm_id` is therefore never treated as
  authoritative identity, never becomes a `PERM-{perm_id}` venue id, and no fallback venue identity is
  synthesized for a completed record.
- `account_id` is the configured Nautilus account identity, and the record is accepted **only** when its
  broker account matches that account exactly. A completed record whose broker account is empty is
  rejected: an absent broker account is never silently replaced by the local account. A foreign broker
  account is rejected as well. No report is emitted that invents account provenance.
- `client_order_id` is the normalized client `order_ref`, preserved exactly so downstream matching still
  binds to the order this client submitted. A completed record with an empty or otherwise unusable
  `order_ref` is rejected before publication: `perm_id`, the live API order id and local OMS identity are
  never substituted for the broker-provided client identity.

Completed-order status handling is terminal-only:

- accepted terminal statuses: `Cancelled`, `ApiCancelled` (both `CANCELED`), `Filled` (`FILLED`),
  `Inactive` (`REJECTED`);
- rejected known non-terminal statuses: `ApiPending`, `PendingSubmit`, `PreSubmitted`, `Submitted`,
  `PendingCancel` - a completed record reporting a working state is contradictory broker evidence and is
  not promoted into terminal authority;
- rejected unknown raw statuses: the raw string is unmappable, so no report is produced and nothing is
  defaulted to a non-terminal status;
- the raw IBKR status string is preserved in `raw_order_status` for every accepted report;
- an accepted terminal status is never downgraded to `PartiallyFilled` by the working-order partial-fill
  rule, so an order cancelled after a partial fill is reported as `CANCELED`.

Overlap and deduplication: IBKR can return the same order from both sources in one window, and the two
observations can disagree about venue identity, because IBKR reports `perm_id == 0` until it acknowledges
an order (an open-order observation may therefore carry only the API order-id fallback while the
completed-order observation for the same order carries a real `perm_id`). Observations are correlated on:

1. the Nautilus venue identity (`VenueOrderId`) derived from the broker `perm_id`; and
2. only while exactly one of the two observations lacks broker venue identity, the `client_order_id`
   derived from the broker `order_ref`.

Two observations that both carry a positive broker `perm_id` are never merged on client identity, so
unrelated broker orders cannot be collapsed merely because one field matches; two distinct broker venue
orders sharing one client identity are both kept. The API client id and the broker account are not
correlation keys (every order of the session shares them), and instrument, price or quantity are never
used. Precedence is deterministic: stronger broker venue identity replaces the API order-id fallback,
terminal broker evidence replaces a non-terminal observation, and between two broker-identified terminal
observations the completed-order observation wins. Ordering is deterministic: open-order observations
keep their source position (a replacement overwrites in place) and completed-only observations are
appended in broker arrival order, so one snapshot never carries contradictory reports for one correlated
order.

Fail-closed behavior: if the completed-order request fails, times out, or the connected server does not
support it, the snapshot carries no completed-order evidence and keeps the open-order observations
unchanged. A missing completed record is never interpreted as cancellation, and no terminal state is ever
inferred solely from absence in the open-order response.

## Troubleshooting

- Confirm TWS or IB Gateway is running and logged in.
- Confirm socket API access is enabled and the configured port matches the application and trading
  mode.
- Confirm the API client ID is not already in use.
- Confirm the account has the required market data subscriptions. Use
  `MarketDataType.DELAYED` only when delayed data is acceptable.

For IB error codes and connection settings, see the
[official TWS API reference](https://ibkrcampus.com/campus/ibkr-api-page/twsapi-doc/).

## Contributing

For additional features or to contribute to the Interactive Brokers adapter, see the
[contributing guide](https://github.com/nautechsystems/nautilus_trader/blob/develop/CONTRIBUTING.md).
