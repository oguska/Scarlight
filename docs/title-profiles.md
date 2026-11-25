# Title profiles

A profile is what **one title's build** needs from this layer beyond the generic path: a
code patch to place, shaders to leave out, a label to print. Profiles belong to the user,
not to the repository: they are read at launch from a directory outside the tree, and one
is selected by the title's own identity — the BLAKE3 of the image we are about to run.

Nothing in the code knows a title. The loader hashes the image, asks "is there a profile
for this build?", and the loader, the HLE and the GPU layer then read that profile. No
title means no profile, and the generic path runs unchanged.

## Where profiles live

In order of precedence:

1. `VICTORY_TITLES_DIR` if set;
2. `[paths].titles_dir` in `victory.toml`;
3. `titles/` next to the `victory.toml` that was read;
4. the user's data directory: `%LOCALAPPDATA%\victory\titles` (`$XDG_DATA_HOME` or
   `~/.local/share/victory/titles` elsewhere).

Every `*.toml` in that directory is read. A file that does not parse is logged and skipped;
one bad profile must not keep a title from running.

**A profile is user data, never repository data.** It may name hashes of the title's files,
per-build offsets and shader hashes — all of which `docs/asset-policy.md` keeps out of the
tree. Do not open a pull request that adds one.

## Fields

```toml
# <profiles dir>/<any name>.toml
name  = "my label for this build"   # what the layer prints at launch
build = ["<hex>"]                   # BLAKE3 of the image, 16+ digits, matched as a prefix

[[hooks]]                           # optional; place code patches after mapping
what   = "our words for the patch"  # logged
rva    = 0x1000                     # offset from the start of the mapped image
expect = "488b0500000000"           # bytes that must be there first (hex, no spaces)
new    = "eb18909090909090"         # bytes to write (same length as expect)

[skip]                              # optional; work the GPU layer leaves out
shaders = ["<16 hex digits>"]       # skip a pipeline containing one of these shaders
stages  = ["mesh", "compute"]       # skip 64-lane programs of these stages
```

A hook is only applied when `expect` matches exactly, so a profile that names the wrong
build changes nothing. Hooks need `VICTORY_HOOKS=1` (formerly `VICTORY_NO_WATCHDOG=1`,
still accepted) — patches are opt-in, never automatic.

## What a profile cannot do

- Add title knowledge to the code: the mechanism is the same for every title, and a
  feature that exists for one title's build has no place in the tree unless it generalises.
- Ship with VIctory: nothing in the repository, its docs or its tests names a hash, an
  offset or a shader of any title.

See `docs/asset-policy.md` for why, and `victory.example.toml` for the paths setting.
