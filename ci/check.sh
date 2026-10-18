#!/bin/bash
set -uo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." || exit 2

linux_features=gpui-pre-platform/wayland,gpui-pre-platform/x11,gpui-pre-platform/font-kit,gpui-pre-platform/runtime_shaders
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

run_gate cargo fmt --check
run_gate cargo clippy --workspace --all-targets --locked --features "$linux_features" -- -D warnings
run_gate cargo test --locked -p gpui-pre --features test-support --test uniform_list_fixed_size
run_gate cargo test --workspace --locked --features "$linux_features"

if command -v cargo-deny >/dev/null 2>&1; then
    run_gate cargo deny check licenses
else
    printf 'SKIP: cargo-deny is not installed; license check did not run.\n'
fi

exit "$status"
