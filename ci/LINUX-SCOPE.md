# Supported platform and dependency scope

This fork supports Linux and FreeBSD. The platform crate has no macOS,
Windows or Web backend dependencies. Compatibility feature names remain
available where their removed backend reference was their only effect.
The Linux/FreeBSD and optional Linux WGPU dependency tables are unchanged.

All GPUI workspace dependencies use sibling paths. The optional benchmark
metrics crate is the exact published Apache-2.0 `gpui-pre-bench-metrics`
0.3.9 source at Zed revision `93d745668805029476e6d7fbb19652116fed51de`.
It keeps the existing benchmark contexts and platform benchmarks usable
without resolving a second GPUI source. Its license accompanies the source.
Tracing diagnostics use the crate's existing `tracing` dependency instead
of the optional registry `gpui-pre-zlog` dependency.

Validate source scope with:

```sh
cargo tree --locked --workspace --target all --all-features
```

Every GPUI entry must resolve inside this workspace. Run `ci/check.sh`
for the supported Linux CI feature set and its documented inherited test
exclusions in `ci/UPSTREAM-EXCLUSIONS.md`.
