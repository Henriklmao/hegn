# hegn vendor patches for tirith-core 0.4.2

Upstream `tirith-core` 0.4.2 is the newest release, but its dependency set
contains several RustSec advisories. This vendored copy keeps the rule engine
unchanged and only updates the affected dependencies:

| Dependency | Upstream | Vendored | Reason |
| --- | --- | --- | --- |
| `hickory-resolver` | 0.24.4 | 0.26.1 | RUSTSEC-2026-0119 in `hickory-proto` |
| `lopdf` | 0.34 | 0.42.0 | RUSTSEC-2026-0187 |
| `zip` | 2.x | 9.x | RUSTSEC-2026-0367/0368/0369 |
| `serde_yaml` | 0.9 (deprecated) | `serde_yaml_ng` 0.10 | maintained fork |

Source changes are limited to the API adjustments required by those upgrades:

- `network/dns.rs`: `TokioResolver::builder_tokio()` instead of
  `TokioAsyncResolver::tokio_from_system_conf()`.
- `rules/rendered.rs`: `lopdf::Error::DictKey` is now a tuple variant; PDF
  filters are compared as byte slices.
- `artifact/archive.rs`: `zip::ZipFile::name()` returns `Result`; undecodable
  member names are recorded as malformed archives.
- `threatdb_feeds.rs`: same `ZipFile::name()` adjustment.

The crate remains licensed under AGPL-3.0-only; see `LICENSE-AGPL`.
