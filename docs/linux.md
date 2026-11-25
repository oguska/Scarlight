# Linux setup (Mesa)

Tested on one machine: CachyOS (Arch family), RX 9070 XT, Mesa 26.3.1 RADV, GE-Proton11-14. Other
Mesa GPUs (RADV generations, ANV, NVK) are untested. Read the [status](../README.md#status) first:
this is not playable.

## 1. Packages

```bash
sudo pacman -S --needed mesa vulkan-radeon rustup git clang lld llvm   # vulkan-intel on Intel
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
PROTON="$HOME/.local/share/Steam/compatibilitytools.d/GE-Proton11-14"   # your folder name
EXE="$HOME/src/Scarlight/target/x86_64-pc-windows-msvc/release/victory.exe"
GAME_DIR="$HOME/victory-data/game"          # your own legally obtained files
GAME_EXE="<the title's exe>"

export WINEPREFIX="$HOME/.local/share/scarlight-prefix"
export WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3d11,d3d10core,d3dcompiler_47=n,b"
export VKD3D_CONFIG=ignore_shared_fence
export VICTORY_NO_WATCHDOG=1   # builds that carry the hook; see the README switches table
export VICTORY_GAME_DIR="$GAME_DIR"
export VICTORY_LOG="${VICTORY_LOG:-info}"

exec "$PROTON/files/bin/wine" "$EXE" run "$GAME_DIR/$GAME_EXE" --execute
```

Make it executable once with `chmod +x play.sh`. Pass switches in front: `VICTORY_PIPELINE_WAIT_MS=24 ./play.sh`. The switches are listed in [the README](../README.md#switches).
If you use GE-Proton's own D3D12 DLLs, make sure your prefix has them installed or copy them next
to the exe.

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
| The title quits while waiting for the GPU | `VICTORY_NO_WATCHDOG=1` — only builds that carry the hook; see the [README switches](../README.md#switches) |
| Freezes | Unset all experimental switches |
| `victory` reports no paths | `victory config`; paths must be outside the repository |
| RenderDoc installed and the title crashes | `VK_LOADER_LAYERS_DISABLE='*renderdoc*'` |

Crashes and GPU failures are appended to `crashes.log` in the data folder.

For more diagnostics, build on Linux with:

```bash
cargo xwin build --profile diag -p victory --features diagnostics --target x86_64-pc-windows-msvc
```

Then point `EXE` in `play.sh` at `target/x86_64-pc-windows-msvc/diag/victory.exe`, with the same
[files next to it](#3-files-next-to-victoryexe). The switches are listed in
[the README](../README.md#switches).
