# Inherited license baseline

The global allowlist is Apache-2.0, MIT, BSD-2-Clause, BSD-3-Clause, and Zlib.
The existing graph on parent `14f661bcddd6afa8b068729b5cb07d7a04710ffe`
also requires the exact-version baseline entries below. No dependency was added
or upgraded for this unit. `../28-parent-licenses.log` reproduces every rejected
entry with the same global allowlist. `../parent-lock-provenance.txt` records the
version-preserving parent resolution.

These are named exceptions for the inherited graph, not a global allowance for
these license identifiers. A different crate or version must satisfy the global
allowlist or fail the check. The complete graph, including development and build
dependencies, remains checked. `ci/check.sh` announces this baseline explicitly.

- `cbindgen` `=0.28.0`: `MPL-2.0`.
- `dwrote` `=0.11.5`: `MPL-2.0`.
- `hexf-parse` `=0.2.1`: `CC0-1.0`.
- `icu_collections` `=2.3.0`: `Unicode-3.0`.
- `icu_locale_core` `=2.3.0`: `Unicode-3.0`.
- `icu_normalizer` `=2.3.0`: `Unicode-3.0`.
- `icu_normalizer_data` `=2.3.0`: `Unicode-3.0`.
- `icu_properties` `=2.3.0`: `Unicode-3.0`.
- `icu_properties_data` `=2.3.0`: `Unicode-3.0`.
- `icu_provider` `=2.3.1`: `Unicode-3.0`.
- `libloading` `=0.8.9`: `ISC`.
- `litemap` `=0.8.3`: `Unicode-3.0`.
- `option-ext` `=0.2.0`: `MPL-2.0`.
- `potential_utf` `=0.1.6`: `Unicode-3.0`.
- `tiny-keccak` `=2.0.2`: `CC0-1.0`.
- `tinystr` `=0.8.4`: `Unicode-3.0`.
- `unicode-ident` `=1.0.27`: `Unicode-3.0`.
- `writeable` `=0.6.4`: `Unicode-3.0`.
- `yoke` `=0.8.3`: `Unicode-3.0`.
- `yoke-derive` `=0.8.4`: `Unicode-3.0`.
- `zerofrom` `=0.1.8`: `Unicode-3.0`.
- `zerofrom-derive` `=0.1.8`: `Unicode-3.0`.
- `zerotrie` `=0.2.5`: `Unicode-3.0`.
- `zerovec` `=0.11.8`: `Unicode-3.0`.
- `zerovec-derive` `=0.11.6`: `Unicode-3.0`.

Revisit these entries when rebasing upstream. Never widen a version requirement
or add a new crate to the baseline without its own provenance and review.
