# Upstream

This tree starts from the `gpui-pre` 0.3.9 crates on crates.io, which are snapshots of
Zed's GPUI at `zed-industries/zed@93d7456` (Apache-2.0). The first commit is that snapshot
unmodified; every later commit is an Omarchy change on top of it.

To move to a newer snapshot, import it as a new pristine commit on a branch and rebase
the Omarchy commits onto it.
