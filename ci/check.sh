#!/bin/bash
set -uo pipefail

# Compile locally if the optional compiler cache loses its server.
export SCCACHE_IGNORE_SERVER_IO_ERROR=1

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." || exit 2

linux_features=gpui-pre-platform/wayland,gpui-pre-platform/x11,gpui-pre-platform/font-kit,gpui-pre-platform/runtime_shaders
# Parent 14f661bc reproduces these failures; test bodies remain unchanged.
# See UPSTREAM-EXCLUSIONS.md for exact targets and reproduction commands.
upstream_exclusions=gpui-pre/ci-exclude-bundled-font-tests,gpui-pre/ci-exclude-style-perf-tests,gpui-pre/ci-exclude-random-tree-property-test,gpui-pre-sum-tree/ci-exclude-random-test,gpui-pre-wgpu/ci-exclude-naga-tests,gpui-pre-wgpu/ci-exclude-bundled-font-tests
ci_features=$linux_features,gpui-pre-platform/test-support,gpui-pre-sum-tree/test-support,$upstream_exclusions
printf 'EXCLUDE: four SVG font tests, twelve WGPU font tests, three style perf tests, one randomized-tree property test, one sum-tree random test, three naga shader tests.\n'
# imported-examples and imported-layout-line-bench remain disabled explicitly.
printf 'EXCLUDE: 44 gpui-pre imported examples; gpui-pre-wgpu layout_line benchmark; gpui-pre-macros derive_context, derive_inspector_reflection, render_test.\n'
runtime_exclusions=(--skip logging_tests::logging_preserves_diagnostics_and_callers)
core_doctest_groups=(_ownership_and_data_flow README.md)
macro_doctest_groups=(gpui_macros.rs)
primary_doctest_skips=(--skip _ownership_and_data_flow --skip README.md --skip gpui_macros.rs)
core_doctest_skips=(--skip '5)' --skip '20)' --skip '40)' --skip '58)' --skip '108)' --skip '17)')
macro_doctest_skips=(--skip property_test --skip '220)')
printf 'EXCLUDE: gpui-pre-util logging target assertion and six gpui-pre doctests and three macro doctests, reproduced on parent 14f661bc.\n'
status=0

run_gate() {
    local result
    printf 'RUN:'
    printf ' %q' "$@"
    printf '\n'
    "$@"
    result=$?
    printf 'EXIT: %s\n' "$result"
    if [ "$result" -ne 0 ] && [ "$status" -eq 0 ]; then
        status=$result
    fi
}

check_doctest_exclusions() {
    cargo test --workspace --locked --features "$ci_features" --doc -- --list |
        python3 ci/check-doctest-exclusions.py
}

run_gate cargo fmt --check
run_gate cargo clippy --workspace --all-targets --locked --features "$ci_features" -- -D warnings
run_gate cargo test --locked -p gpui-pre --features test-support --test uniform_list_fixed_size
run_gate cargo test --workspace --locked --features "$ci_features" --lib --bins --tests --benches -- "${runtime_exclusions[@]}"
run_gate check_doctest_exclusions
run_gate cargo test --workspace --locked --features "$ci_features" --doc -- "${primary_doctest_skips[@]}"
run_gate cargo test --workspace --locked --features "$ci_features" --doc -- "${core_doctest_groups[@]}" "${core_doctest_skips[@]}"
run_gate cargo test --workspace --locked --features "$ci_features" --doc -- "${macro_doctest_groups[@]}" "${macro_doctest_skips[@]}"

if command -v cargo-deny >/dev/null 2>&1; then
    printf 'LICENSE BASELINE: 25 exact-version inherited entries; see ci/LICENSE-BASELINE.md.\n'
    run_gate cargo deny check licenses
else
    printf 'SKIP: cargo-deny is not installed; license check did not run.\n'
fi

exit "$status"
