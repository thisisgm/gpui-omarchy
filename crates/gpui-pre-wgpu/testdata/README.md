# Font fixture

`JetBrainsMono-Regular.ttf` is JetBrains Mono Regular 2.304 as built by Debian's fonts-jetbrains-mono 2.304+ds-5 package, copied unmodified from its `/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf`. Debian removes upstream's prebuilt fonts and builds them from the upstream source at https://github.com/JetBrains/JetBrainsMono, so refresh this fixture from that package, not from an upstream release. SHA256: `9fd21a629ec73a93d1665b6d8bbb73928040a290b49d2f6772505b8129e68cdc`.

The font's own name table licenses it under the SIL Open Font License 1.1; that license and the font's copyright line are in `LICENSE-JetBrainsMono.txt`. Only tests read it: the loaded-font export and supplied-font SVG tests use it so they do not depend on fonts installed on the test host.
