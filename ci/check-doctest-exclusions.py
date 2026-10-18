#!/usr/bin/env python3
"""Verify that whitespace-free rustdoc filters exclude only the proven cases."""

from pathlib import Path
import re
import shlex
import sys

EXPECTED = {
    "crates/gpui-pre/src/../README.md - (line 17)",
    "crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 5)",
    "crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 20)",
    "crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 40)",
    "crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 58)",
    "crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 108)",
    "crates/gpui-pre-macros/src/gpui_macros.rs - property_test (line 300)",
    "crates/gpui-pre-macros/src/gpui_macros.rs - property_test (line 343)",
    "crates/gpui-pre-macros/src/gpui_macros.rs - test (line 220)",
}
source = Path(__file__).with_name("check.sh").read_text()


def array(name):
    match = re.search(r"^" + name + r"=\((.*)\)$", source, re.M)
    if not match:
        sys.exit("CI filter array must be a literal single line: " + name)
    values = tuple(shlex.split(match[1]))
    if any("$" in value for value in values):
        sys.exit("CI filter array must not use shell expansion: " + name)
    return values


def skips(name):
    values = array(name)
    if len(values) % 2 or any(flag != "--skip" for flag in values[::2]):
        sys.exit("Unexpected CI skip arguments: " + name)
    return values[1::2]


primary_skips = skips("primary_doctest_skips")
core_groups = array("core_doctest_groups")
macro_groups = array("macro_doctest_groups")
core_skips = skips("core_doctest_skips")
macro_skips = skips("macro_doctest_skips")

names = set()
for line in sys.stdin:
    match = re.fullmatch(r"(.+\(line \d+\)(?: - compile(?: fail)?)?): test\s*", line)
    if match:
        name = re.sub(r" - compile(?: fail)?$", "", match[1])
        if name in names:
            sys.exit("Duplicate doctest name: " + name)
        names.add(name)
    elif line.rstrip().endswith(": test"):
        sys.exit("Unrecognized doctest inventory entry: " + line.rstrip())

if not EXPECTED <= names:
    sys.exit("Doctest baseline entries are missing: " + repr(sorted(EXPECTED - names)))

primary = {name for name in names if not any(group in name for group in primary_skips)}
core = {name for name in names if any(group in name for group in core_groups)}
macros = {name for name in names if any(group in name for group in macro_groups)}
core_skipped = {name for name in core if any(token in name for token in core_skips)}
macros_skipped = {name for name in macros if any(token in name for token in macro_skips)}
excluded = core_skipped | macros_skipped
if excluded != EXPECTED:
    sys.exit("Doctest filters differ from the named baseline: " + repr(sorted(excluded ^ EXPECTED)))

covered = primary | (core - core_skipped) | (macros - macros_skipped)
if covered != names - EXPECTED:
    sys.exit("Non-excluded doctests are missing from the CI runs")

print(f"PASS: {len(EXPECTED)} named doctest exclusions; all {len(covered)} other examples remain covered.")
