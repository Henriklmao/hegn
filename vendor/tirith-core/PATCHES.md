# hegn vendor patches for tirith-core 0.5.0

Upstream `tirith-core` 0.5.0 is the newest release, but its dependency set still
contains the RustSec advisories that upstream knowingly ignores in its own
`deny.toml`: it pins `rust-version = "1.83"`, and every fixed dependency line
requires a newer compiler. hegn pins a newer Rust than that, so it can take the
fixes that upstream cannot. This vendored copy keeps the rule engine unchanged
and only updates the affected dependencies:

| Dependency | Upstream | Vendored | Reason |
| --- | --- | --- | --- |
| `hickory-resolver` | 0.24.4 | 0.26.1 | RUSTSEC-2026-0119 in `hickory-proto` |
| `lopdf` | 0.34 | 0.42.0 | RUSTSEC-2026-0187 |
| `zip` | 2.x | 9.x | RUSTSEC-2026-0367/0368/0369/0429 |
| `serde_yaml` | 0.9 (deprecated) | `serde_yaml_ng` 0.10 | maintains the parser on a supported fork, removing the unmaintained `unsafe-libyaml` advisory for this dependency |

Source changes are limited to the API adjustments required by those upgrades:

- `network/dns.rs`: `TokioResolver::builder_tokio()` instead of
  `TokioAsyncResolver::tokio_from_system_conf()`.
- `rules/rendered.rs`: `lopdf::Error::DictKey` is a tuple variant; PDF stream
  filters are compared as byte slices.
- `artifact/archive.rs`: `zip::ZipFile::name()` returns `Result`; undecodable
  member names are recorded as malformed archives.
- `threatdb_feeds.rs`: same `ZipFile::name()` adjustment.

`hickory-resolver` 0.26 replaces the `tokio-runtime` feature with `tokio`; the
standalone crate manifest enables `system-config` plus `tokio`. Upstream's
`[[test]]` and `[[bench]]` targets are dropped from the vendored manifest
because the `tests/` and `benches/` trees are not vendored.

`src/security/engine.rs` in the main crate passes the three `AnalysisContext`
fields 0.5.0 added (`python_inspect_inherited`, `cdpath_inherited`,
`interactive_comments`): hegn inspects a command before it reaches the guest
shell, so it never inherits the host's `PYTHONINSPECT`/`CDPATH`, and the guest
shell is an interactive POSIX shell where `#` starts a comment.

The crate remains licensed under AGPL-3.0-only; see `LICENSE-AGPL`. `NOTICE`
is upstream's and is shipped unmodified.
