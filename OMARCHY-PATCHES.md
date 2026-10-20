# Omarchy patches

The baseline is the gpui-pre 0.3.9 snapshot of Zed GPUI at
`93d745668805029476e6d7fbb19652116fed51de`. These changes are absent from that
snapshot. Upstream submission status has not been recorded.

Each entry gives the change, commit, measured effect, evidence, and upstream status.

- Vulkan first with OpenGL fallback | `ddf3dbacacd7e67ea7ad91d4c490e85dedb88523` | Avoids EGL initialization when Vulkan succeeds; no timing range retained in the imported history | Evidence: that commit's message and `crates/gpui-pre-wgpu/src/wgpu_context.rs`, `crates/gpui-pre-wgpu/src/wgpu_renderer.rs` | Fork only; submission unrecorded.
- Lazy path textures | `f8d8890fb25d6cd014f6beb6db6dab2aedc1ebb5` | Approximately 70 MB less GPU memory at 2536x1386 for scenes without paths; no measurement range retained | Evidence: that commit's message and `crates/gpui-pre-wgpu/src/wgpu_renderer.rs` | Fork only; submission unrecorded.
- On-demand fontconfig fonts | `14f661bcddd6afa8b068729b5cb07d7a04710ffe` | Avoids parsing 800 installed font files at startup, previously about 720 ms on the reference machine; no measurement range retained | Evidence: that commit's message and `crates/gpui-pre-wgpu/src/fontconfig_on_demand.rs` | Fork only; submission unrecorded.
- Fixed uniform-list item size | `b29801a088aa5696f6729325d7ee26f5699a309b` | Prior campaign saved 0.02 to 0.24 ms per frame with neutral memory; layout parity covers empty, short, long, scrolled and horizontally unconstrained lists | Evidence: `/home/gm/flea-ops/gpui-omarchy-20261016/idiom-work/fixed-campaign-r1-evidence.tar` and `../06-fixed-size-standalone.log` | Fork only; submission unrecorded.
- Intra-fork path dependencies | `d35a2d5502857368e8b9cb6907e1a791b7c2e1b1` | No runtime change; all 15 active gpui-pre crates in the Linux consumer tree resolve to this fork without a patch table | Evidence: `../consumer-tree.log`, `../consumer-proof.txt`, and `Cargo.lock` | Fork only; submission unrecorded.
- Glyph runs keep atlas tiles between frames | `f3a4bc78f57eb1dad9802286253cea22284e5617`, with fix `d38b68a4f95a23e1d7c8dae9003a09866e46ad80` | A line painted again with the same x, scale, rendering mode, dilation and atlas generation reuses its glyphs' raster bounds and tiles; removing a glyph's tile advances the generation, removing an image or SVG tile does not; Flea list bench thread CPU per frame on East fell from 305..333 to 165..176 us (bounds set), 116..131 to 65..78 us (2k set) and 107..120 to 61..67 us (uniform set) with equal allocations | Evidence: both commits' messages, `crates/gpui-pre/src/window/glyph_run_tests.rs`, the atlas generation tests in `crates/gpui-pre-wgpu/src/wgpu_atlas.rs`, and Flea g08 r2 rounds logs `base1.log`, `base2.log`, `r4b.log` | Fork only; submission unrecorded.

- MIME-aware Linux clipboard and Wayland drag | Pending integration commit | Synthetic 1,000-path MIME serialization: 630 to 1,844 microseconds over 25 debug samples, excluding native transport, frame time and memory | Evidence: `crates/gpui-pre-linux/src/linux/transfer.rs` cost test, Wayland offer fakes, X11 selection fakes and `docs/native-mime-transfers.md` | Fork only; native interoperability and independent review pending.
- Keypad identity on Linux keystrokes | `70513ccc6c36f4fa283f5a1e42ff864991025e6f` | Not timed; adds one keysym range check per key event; `Keystroke::is_keypad` is set for every KP_ keysym on Wayland and X11 while `key`, `key_char` and binding matching stay unchanged | Evidence: the keypad tests in `crates/gpui-pre-linux/src/linux/x11/client.rs`, `crates/gpui-pre-linux/src/linux/wayland/client.rs` and `crates/gpui-pre/src/keymap.rs` | Fork only; submission unrecorded.
- Presented-frame feedback on Wayland | `c4cf476bf984a7585eff336f963153e83e485544`, with fix `1cf89511ef27134136d168e2ae94d8ebc561715f` | One wp_presentation feedback per commit, numbered only when its present succeeds; `Window::presented_frames` and `observe_frame_presented` advance only on presented and report `None` without wp_presentation and on X11; client cost against a socketpair compositor stand-in, 10 batches of 10,000 frames on West: request 344 to 352 ns and dispatch 195 to 200 ns with 3 allocations for the object, beside 291 to 294 ns and 168 to 170 ns for the existing frame callback | Evidence: the tests in `crates/gpui-pre-linux/src/linux/wayland/presentation.rs` and g0-fork2 `feedback-cost-1.log` | Fork only; a native compositor run and submission are unrecorded.
- Text-input-v3 IME on Wayland | `a77ba5d01fb6b39ce1102e8df4b67e53d1ce7e3c`, `d1af7dfa4bf4a6e17b6f5d9446388257fd3bf13b`, `557fb4edbe8042094839832cea226722b69c83d0` and `12c86bfc83c94fd7be448971357d4b42bcfa0408`, with fixes `a0935131b3ca4e301cb5efaa6d6d3c5b24648607` and `28e93c607a9d2049c466797925253eba0be48806` and tests `3ae650fdb7a9c23906fb47c6d4d58daba2588dfa`, `9dc12cf9047863f925c63fafdba10d70d034f6a0`, `c8b7d3089f18ffde06dcbe5bec58d42f89760a72`, `42879f5ff25390a16bd5e6eebd3887aa0cca8741`, `ee166f534ba64b9a6527e715d506ac1a50aae0ca`, `8f69cad5e86f4642f01a27ce089ce0927f90f019`, `52841f36856bf40f741c39a675e1ca58fdd7207f` and `4cec4236de8ad3762fbb710ab1f1ef66ec1902e8` | Not timed; a batch applies at done in the protocol's order on the entered surface only, a caret or context change sends one commit, and a context read copies the selection and preedit plus 1,000 UTF-16 units each side, sending at most 4,000 bytes | Evidence: the session, field and event tests in `crates/gpui-pre-linux/src/linux/wayland/ime.rs`, `ime_context.rs` and `ime_events.rs`, the last driving the same event glue the client calls | Fork only; a native fcitx5 run and submission are unrecorded.

## Branches and pins

Start one branch per change unit from `main`, with one named change per commit.
Commit as `GM <gianmarcomorales@icloud.com>`, explaining why the change is needed
and recording measurement ranges and evidence when available. Independently review
each unit before integration. Only the integration owner merges to `main`, after
independent review and green fork CI.

Consumers use `git = "https://github.com/thisisgm/gpui-omarchy"` with a full commit
SHA in `rev`. Advance a consumer pin only after that commit is published on `main`.
Consumers must not require a patch table, a vendored copy, or local GPUI edits.
Sibling path dependencies retain their versions so Cargo can resolve the same
fork graph from a git dependency.

## Upstream rebases

Import a new pristine upstream snapshot on a separate branch and rebase the fork
changes onto it. Keep each change identifiable and update this ledger's SHAs.
Do not adopt a rebase until fork CI and the complete application acceptance
benchmark suite have been rerun against the previous pin on the same hardware,
toolchain, features, and scene. Recheck cold and warm startup, frame CPU time,
memory, font loading, path texture allocation, and list layout. Preserve raw
evidence and record ranges, including regressions, before advancing any pin.
