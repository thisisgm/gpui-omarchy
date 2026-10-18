# Imported test exclusions

These failures reproduce on parent commit
`14f661bcddd6afa8b068729b5cb07d7a04710ffe`, before the foundation changes.
The parent comparison retains every dependency version from the candidate
lockfile. Its workspace patch table removes 13 duplicate registry package IDs;
no package or version was added or upgraded. See
`../parent-lock-provenance.txt` and `../15-parent-unit-locked.log`.

The reproduction command, with `P` naming a pristine parent checkout carrying
that normalized lockfile, is:

```bash
/bin/bash /home/gm/flea-ops/claude/038/bin/qsq.sh @cargo "$P" test --locked -p gpui-pre --features test-support --lib --no-run
```

It exits 101 with both missing-font errors, unresolved `util_macros`, and the
property-test attribute panic. The same errors appear in the candidate's
`../03-fixed-size-test.log`. The parent source diff is empty.

## Missing bundled fonts

CI enables `gpui-pre/ci-exclude-bundled-font-tests`. The imported archive lacks
`assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf` and
`assets/fonts/lilex/Lilex-Regular.ttf`, so `include_bytes!` prevents compilation.
Only these four tests and their fixture helpers are excluded:

- `svg_renderer::tests::text_with_split_glyph_clusters_in_mixed_fonts_does_not_panic`
- `svg_renderer::tests::fix_generic_font_families_sets_all_families`
- `svg_renderer::tests::test_select_emoji_font_skips_family_without_glyph`
- `svg_renderer::tests::fix_generic_font_families_monospace_resolves_to_lilex`

The three SVG tests that do not use those fixtures still compile and run.

## Missing performance-test macro

CI enables `gpui-pre/ci-exclude-style-perf-tests`. The imported manifest has no
`util_macros` dependency, but the style test module imports `util_macros::perf`.
That module contains exactly these three tests:

- `style::tests::test_basic_highlight_style_combination`
- `style::tests::test_combine_highlights`
- `style::tests::test_text_style_refinement`

## Property-test attribute panic

CI enables `gpui-pre/ci-exclude-random-tree-property-test`. The imported
`#[gpui::property_test]` expansion for
`randomized_element_tree::tests::randomized_element_tree_matches_full_render`
panics while parsing with `unexpected token`, expecting a semicolon. Only that test and helpers used
exclusively by it are excluded. The deterministic randomized-tree tests remain.

## Imported example and benchmark targets

Parent `../23-parent-all-targets.log` runs strict workspace Clippy with
`--all-targets --keep-going` and reproduces every target failure listed here.
No production source or test body is changed to hide these errors.

All 44 `gpui-pre` examples refer to the undeclared `gpui_platform` crate;
some also lack `reqwest_client`, `env_logger`, or bundled fonts. Their existing
manifest entries now require the opt-in `imported-examples` feature, which CI
explicitly leaves disabled. The excluded targets are:

- `a11y`
- `active_state_bug`
- `anchor`
- `animation`
- `data_table`
- `drag_drop`
- `focus_visible`
- `gif_viewer`
- `gradient`
- `grid_layout`
- `hello_world`
- `image`
- `image_gallery`
- `image_loading`
- `input`
- `layer_shell`
- `list_example`
- `mouse_pressure`
- `move_entity_between_windows`
- `on_window_close_quit`
- `opacity`
- `ownership_post`
- `painting`
- `paths_bench`
- `pattern`
- `popover`
- `scrollable`
- `set_menus`
- `shadow`
- `svg`
- `system_notifications`
- `tab_stop`
- `testing`
- `text`
- `text_layout`
- `text_wrapper`
- `toggle_headless`
- `tree`
- `uniform_list`
- `view_example`
- `window`
- `window_movable`
- `window_positioning`
- `window_shadow`

`gpui-pre-wgpu::layout_line` requires the missing `criterion` development
dependency and the same missing IBM Plex Sans and Lilex fixtures. Its manifest
entry requires `imported-layout-line-bench`, which CI leaves disabled.

The `gpui-pre-macros` integration targets `derive_context`,
`derive_inspector_reflection`, and `render_test` expand references to an
undeclared `gpui` dependency. Their existing entries require
`imported-integration-tests`, which CI leaves disabled.

New example, benchmark, and integration-test entries remain checked by default.
The exclusions apply only to the named entries above. Library and integration
tests that compile are still checked with `-D warnings` and executed. CI enables
platform and sum-tree `test-support` so their declared test dependencies are
available; missing test-support features are configuration errors, not excluded
tests.

## Sum-tree randomized test

Parent `../26-parent-sum-tree-tests.log` reproduces nine errors in
`sum_tree::tests::test_random` and its shared logger scaffold: `rand`, `ctor`,
`log`, and `zlog` are undeclared. CI enables
`gpui-pre-sum-tree/ci-exclude-random-test` for that one test and its unused
imports/logger. The other four tests in that module and the tree-map tests
remain enabled. Their bodies are unchanged.

## Missing shader-validator dependency

Parent `../23-parent-all-targets.log` reproduces the undeclared `naga`
dependency used by three `wgpu_renderer::tests` cases. CI enables
`gpui-pre-wgpu/ci-exclude-naga-tests` for only:

- `webgl_shader_is_valid_wgsl_without_storage_buffers`
- `storage_buffer_shader_is_valid_wgsl`
- `subpixel_shader_is_valid_wgsl`

Their shared `validate_wgsl` helper is excluded with them. The four headless
renderer tests and two WebGL layout tests remain enabled.

## WGPU bundled-font tests

Parent `../23-parent-all-targets.log` and `../31-parent-wgpu-tests.log`
reproduce missing IBM Plex Sans, Lilex Regular, and Lilex Bold fixtures in
`cosmic_text_system::tests`. CI enables
`gpui-pre-wgpu/ci-exclude-bundled-font-tests` for the twelve fixture-dependent
cases below and their exclusive fixture helpers. The fifteen pure text tests
remain enabled.

- `all_font_names_tracks_available_families`
- `font_properties_describe_the_selected_face`
- `shape_text_with_mixed_direction_paragraphs`
- `reports_graphemes_that_exhaust_font_fallback`
- `combines_missing_glyphs_from_one_grapheme`
- `adding_fonts_invalidates_cached_line_layouts`
- `layout_line_with_mixed_direction_paragraphs`
- `layout_line_with_separators_at_line_edges`
- `layout_line_keeps_indices_and_positions_ordered_across_paragraphs`
- `layout_line_with_font_run_straddling_a_separator`
- `layout_line_without_separators_takes_fast_path`
- `index_for_position_past_line_width`

## Logging target-name assertion

Parent `../35-parent-logging-test.log` reproduces
`logging_tests::logging_preserves_diagnostics_and_callers` failing because the
published package reports `gpui-pre-util::lib` while the imported assertion
expects `gpui_util::lib`. CI passes that exact test name through `--skip`.
The logger test and all its assertions are unchanged; the other util test stays
active. The runtime exclusion is printed explicitly before the workspace run.

## Documentation examples with an undeclared platform crate

Parent `../38-parent-doctests.log` reproduces exactly the same six failed
examples as the candidate `../37-workspace-tests-final.log`. Each refers to the
undeclared `gpui_platform` dependency. CI skips only the following full rustdoc
test names. All remaining doctests still compile and execute.

- `crates/gpui-pre/src/../README.md - (line 17)`
- `crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 108)`
- `crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 20)`
- `crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 40)`
- `crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 5)`
- `crates/gpui-pre/src/_ownership_and_data_flow.rs - _ownership_and_data_flow (line 58)`

## Macro documentation examples

Parent `../42-parent-macro-doctests.log` reproduces the same missing `gpui`
references and missing `TestAppContext` binding as candidate
`../41-workspace-tests-final.log`. CI skips only:

- `crates/gpui-pre-macros/src/gpui_macros.rs - property_test (line 300)`
- `crates/gpui-pre-macros/src/gpui_macros.rs - property_test (line 343)`
- `crates/gpui-pre-macros/src/gpui_macros.rs - test (line 220)`

The three compiling macro doctests remain enabled; two pre-existing ignored
examples retain their original status.

## Preserve every compiling doctest

This rustdoc splits test arguments on whitespace, so a full test name passed to
`--skip` would also remove compiling examples from its source file. CI instead
runs the native targets separately and uses three whitespace-free doctest runs:
all unaffected items, the ownership/README items, and the macro items. Their union
covers every non-excluded example, including the passing ownership example and
all three passing macro examples.

`check-doctest-exclusions.py` checks the complete rustdoc inventory before these
runs. It requires that the filters remove exactly the nine names listed above.
A new example colliding with a filter fails this check instead of being silently
skipped. Test bodies, code-fence annotations, and existing ignore statuses are
unchanged.

## Removal policy

These opt-in features guard test declarations and helper imports. Test bodies
and assertions are unchanged, and none of the features changes production code.
`ci/check.sh` names and prints the exclusions before running its checks. A plain
test build without the features still reproduces the imported failures.

Remove an exclusion only after its original test compiles and passes on both
the updated parent and the fork with the exclusion disabled. Do not replace
fixtures, remove assertions, or change property-test semantics to make CI pass.
