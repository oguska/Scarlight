# Linux setup (Mesa)

Tested on one machine: CachyOS (Arch family), RX 9070 XT, Mesa 26.3.1 RADV, GE-Proton11-14. Other
Mesa GPUs (other RADV generations, ANV) are untested. Read the [status](../README.md#status) first:
this is not playable.

## 1. Packages

```bash
sudo pacman -S --needed mesa vulkan-radeon vulkan-tools rustup git clang lld llvm   # vulkan-intel on Intel
```

Install **GE-Proton11-14** from [proton-ge-custom](https://github.com/GloriousEggroll/proton-ge-custom/releases),
for example into `~/.local/share/Steam/compatibilitytools.d/`.

## 2. Build

Follow [Quick start](../README.md#quick-start).

## 3. Files next to `victory.exe`

In `target/x86_64-pc-windows-msvc/release/`:

| File | Source |
|---|---|
| `dxgi.dll` | GE-Proton's DXVK copy, `files/lib/wine/dxvk/x86_64-windows/` |
| `d3d12.dll`, `d3d12core.dll` | [vkd3d-proton](https://github.com/HansKristian-Work/vkd3d-proton) master, 64-bit. Without them, GE-Proton11-14's own are used |
| `dxcompiler.dll`, `dxil.dll` | DirectXShaderCompiler release, x64 (needed by the quick-start tag) |
| `d3dcompiler_47.dll` | Microsoft's, from software you are licensed to use (stop-gap, because Wine's built-in one rejects some runtime shaders) |

Never commit any of these, or any game file.

## 4. Play script

Save as `play.sh` **outside the repository** and edit the paths. Your game files must also be
outside the repository.

```bash
#!/usr/bin/env bash
set -eu
PROTON="$HOME/.local/share/Steam/compatibilitytools.d/GE-Proton11-14-x86_64"   # your folder name; usually ends in -x86_64
EXE="$HOME/src/Scarlight/target/x86_64-pc-windows-msvc/release/victory.exe"
GAME_DIR="$HOME/victory-data/game"          # your own legally obtained files
GAME_EXE="<the title's exe>"

export WINEPREFIX="$HOME/.local/share/scarlight-prefix"
export WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3d11,d3d10core,d3dcompiler_47=n,b"
export VKD3D_CONFIG=ignore_shared_fence
export VICTORY_NO_WATCHDOG=1   # only on the title build it was written for; see switches below
export VICTORY_GAME_DIR="$GAME_DIR"
export VICTORY_LOG="${VICTORY_LOG:-info}"

exec "$PROTON/files/bin/wine" "$EXE" run "$GAME_DIR/$GAME_EXE" --execute
```

Make it executable once with `chmod +x play.sh`. Pass switches in front: `VICTORY_GPU_TIMING=1 ./play.sh`;
they are listed under [switches](#switches). If you use GE-Proton's own D3D12 DLLs, make sure your
prefix has them installed or copy them next to the exe.

## 5. Controls

A gamepad works. Otherwise the keyboard acts as a pad while the title's window has focus:

| Keys | Pad |
|---|---|
| Enter, Backspace, X, Y | A, B, X, Y |
| Esc, Tab | Menu, View |
| Arrows | D-pad |
| W A S D / I J K L | left / right stick |
| Q / E, Z / C, F / H | shoulders, triggers, stick clicks |

## Troubleshooting

| Problem | Try |
|---|---|
| Crash at world entry | Replace stock vkd3d-proton 3.1 DLLs with master's or GE-Proton11-14's |
| Very slow, swapping | Close apps that hold RAM or GPU memory (local LLM servers, browsers) |
| The title quits while waiting for the GPU | `VICTORY_NO_WATCHDOG=1` — only on the title build the hook was written for; see [switches](#switches) |
| Freezes | Unset all experimental switches |
| `victory` reports no paths | `victory config`; paths must be outside the repository |
| RenderDoc installed and the title crashes | `VK_LOADER_LAYERS_DISABLE='*renderdoc*'` |

Crashes and GPU failures are appended to `crashes.log` in the data folder.

For more diagnostics, build on Linux with:

```bash
cargo xwin build --profile diag -p victory --features diagnostics --target x86_64-pc-windows-msvc
```

Then point `EXE` in `play.sh` at `target/x86_64-pc-windows-msvc/diag/victory.exe`, with the same
[files next to it](#3-files-next-to-victoryexe).

## Switches

| Switch | Effect |
|---|---|
| `VICTORY_NO_WATCHDOG=1` | Turns off the title's watchdog. Only the title build the hook was written for is patched: it checks the executable, and on any other build it changes nothing |
| `VICTORY_ASYNC_WRITEBACK=1` | **Experimental**, off by default; has hung the title in our tests |
| `VICTORY_LOG=info` | Normal per-frame log; `VICTORY_LOG=<target>=debug` for debug output from one area |
| `VICTORY_GPU_TIMING=1` | Diagnostics builds only: runs and waits for each command list on its own, and logs any list over 50 ms with its slowest commands. Serialising the submission changes what you measure; the per-frame copy statistics are logged without it |
| `VICTORY_SHOW_TARGET=<n>` / `busiest` | Diagnostics builds only: shows the nth texture the frame rendered to or wrote, in first-use order — the index a target dump uses — instead of the title's picture; `busiest` picks the displayable target the most draws wrote. A frame with fewer targets shows the title's own picture |

### Diagnostics switches

These need a build with `--features diagnostics` (see above). They are how a bug report gets the
detail to act on.

| Variable | Effect |
|---|---|
| `VICTORY_STACKS=20,60` | Prints every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | Logs a title `u32` when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | Presses keys at those seconds, focus or not |
| `VICTORY_CAPTURE=<dir>` | Saves presented frames (powers of two by default, or the list in `VICTORY_CAPTURE_FRAMES`) |
| `VICTORY_DUMP_TARGETS=1` | With capture on, also saves every render target of those frames, in first-use order |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | Saves title memory to the cache folder on a crash |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | Omits draws and dispatches, indirect ones included, whose pipeline has a listed 16-digit shader hash |
| `VICTORY_SKIP_WAVE64=mesh,compute` | Omits draws and dispatches whose pipeline has a 64-lane program in a listed stage (diagnostic on 32-lane hosts) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | Stops translated shader loops after `n` iterations per lane instead of 2^20; a real loop longer than `n` is cut short too, so 32 is a diagnostic value and 256-1024 a safer probe |
| `VICTORY_D3D12_DEBUG=1` | Host D3D12 debug layer; `VICTORY_D3D12_GBV=1` adds GPU-based validation (slow) |
| `VICTORY_D3D12_DRED=1` | On a GPU failure, logs the allocation hit and the commands in flight |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | Records what the title plays, from our process only |

Crashes and GPU failures are also appended to `crashes.log` in the data folder. Attach the last ~50
lines of the log to a report; never attach game files, screenshots of game content, shader dumps or
hashes of game data ([asset policy](asset-policy.md)).
