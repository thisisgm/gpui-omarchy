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

## Branches and pins

Start one branch per change unit from `main`, with one named change per commit.
Commit as `GM <gianmarcomorales@icloud.com>`, explaining why the change is needed
and recording measurement ranges and evidence when available. Independently review
each unit before integration. Only the integration owner merges to `main`, after
review and CI failures have been resolved or explicitly accepted.

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
