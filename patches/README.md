# Cargo Patches

This directory contains local Cargo patches for third-party crates when the workspace must keep a
specific upstream version but needs a small compatibility fix.

**These patches are temporary. Remove each patch when the upstream crate supports the required
dependency and API versions.**

## Arrow and Parquet

`hypersync-client` 1.4.0 requires `arrow` and `parquet` 57.x. The matching Parquet release depends
on the external `thrift` crate. The local compatibility crates satisfy those version constraints
and re-export the Arrow and Parquet 59.2.0 APIs without copying upstream source.

Remove both compatibility crates when `hypersync-client` supports Arrow and Parquet 59 or later.

## pyo3-stub-gen

`pyo3-stub-gen` stays pinned to `0.20.0` because later versions reject module paths outside the
`pymodule` root. The stub workflow reads `gen_stub_*` module annotations that target
`nautilus_trader` package paths outside the `nautilus_trader._libnautilus` root.

The crate is licensed as `MIT OR Apache-2.0`. The local copy includes the upstream `LICENSE-MIT`
and `LICENSE-APACHE` texts from `Jij-Inc/pyo3-stub-gen`.

The vendored crate path is excluded from pre-commit and Ruff style checks so those checks do not
rewrite upstream files. Keep local edits limited to the compatibility changes listed below.

The local patch keeps `pyo3-stub-gen 0.20.0` buildable with `pyo3 0.29.0`. It changes only the
PyO3 compatibility surface:

- `src/util.rs`: replaces three removed `Bound<PyAny>::downcast::<T>()` calls with
  `cast::<T>()` for `PyDict`, `PyList`, and `PyTuple`.
- `src/exception.rs`: removes the `PyEnvironmentError` and `PyIOError` stub type impls.
  PyO3 0.29 aliases both names to `PyOSError`, so keeping those impls creates duplicate trait
  impls for the same concrete type.

The patch does not intentionally change generated stub layout, class relocation, module naming, or
signature normalization. Those behaviors stay controlled by `python/generate_stubs.py` and the
pinned `pyo3-stub-gen 0.20.0` code.

Do not update `pyo3-stub-gen` or remove this patch until stub generation no longer depends on the
package module paths outside the `pymodule` root.


## ibapi

`ibapi` stays pinned to 3.3.0 (upstream commit `b140b312d1136d240f3b5651a89f4778e01aa222`,
MIT license, source: `https://github.com/wboayue/rust-ibapi`). Public `ibapi` 4.0.0 still uses
a strict order-status enum and is not a substitute; do not upgrade in C2.6.

The local copy makes one narrowly scoped change: `OrderStatusKind` gains an
`Unknown(String)` variant so an order-status string outside the known nine-value vocabulary
decodes successfully with its raw value preserved byte-for-byte (C2.6). Behavioral delta:

- `OrderStatusKind::Unknown(String)` carries the exact original status string;
- `Copy` is removed from `OrderStatusKind` (`Unknown(String)` owns a `String`);
- `as_str()` returns `&str`; known variants keep their exact wire strings, `Unknown`
  returns the stored original;
- `FromStr` maps unknown non-empty strings to `Unknown(original)` instead of
  `Error::Parse`, so an unknown status no longer terminates the order-update
  subscription; missing/empty required fields still fail as `Error::Parse`
  (message integrity handling is unchanged);
- `Display` / `ToField` / serde keep round-tripping; serialization of the nine known
  variants is unchanged (`Unknown` serializes as `{"Unknown": "..."}`);
- `is_active()` / `is_terminal()` return `false` for `Unknown` (unknown is neither).

Vendor hygiene (offline build constraints; no behavioral delta):

- the optional `utoipa` dependency/feature is removed — the workspace never enables it
  and its registry index metadata is not available offline; the
  `cfg_attr(feature = "utoipa", ...)` attributes were stripped accordingly and the
  utoipa-only `schema_derives_work` test module in `src/contracts/mod.rs` was removed;
- example targets are removed from the vendored manifest (their sources are not
  vendored and the workspace never builds them);
- dev-dependencies `serial_test` and `temp-env` are removed because they are not
  available in the offline registry cache; the affected upstream tests were adapted
  mechanically (`src/transport/recorder_tests.rs` uses a local env-var guard; the trace
  tests drop the `#[serial]` attribute — run the vendored test suite with
  `--test-threads=1` to preserve the serialization guarantee).

Remove this patch when upstream `ibapi` supports preserving unknown order-status
strings through decode in the pinned major line.
