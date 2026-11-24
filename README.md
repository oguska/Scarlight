# Scarlight

Scarlight runs an Xbox Series X|S title's own x86-64 code on a PC, using the files you supply. It
loads the title's executable, answers its console-only calls, and turns its GPU commands and
shaders into Direct3D 12 for your graphics card. The focus is **AMD RDNA4 on Linux under
Wine/Proton**; Windows works too. Scarlight is based on
[VIctory](https://github.com/Marvisak/VIctory) by Marvis and its contributors (GPL-3.0, history
kept). It adds tested fixes from VIctory's open
pull requests and forks, each merged with its author's credit ([INTEGRATION.md](INTEGRATION.md)),
plus its own. No game files, dumps or screenshots are in this repository.

## Status

**Not playable yet.** In short:

| Works | Does not |
|---|---|
| Boots to the menus | The world picture: mostly black |
| Menus at 30 fps | World frame rate: about 0.5 fps |
| Loads the world, with audio | Any real gameplay |

Running on Linux under Proton (or system Wine), with the switches for the world's faults, is
described in [docs/linux-proton.md](docs/linux-proton.md).

The black picture comes from shaders that Scarlight cannot translate yet, so their draws are
skipped. The frame rate is limited by copying the title's memory. Both are being worked on
upstream. Tested setup and measurements: [AMD-LINUX.md](AMD-LINUX.md).

<!-- state:begin -->
## Project state

Last updated: 24 Nov 2026, Scarlight `fe552e4`.

### Milestones

| | Milestone | Note |
|---|---|---|
| ✅ | Boots | Boots into the title's own menus |
| ✅ | Menus | 30 fps, drawn and interactive |
| ✅ | Loads world | World streams in and the title's audio plays |
| 🟡 | World picture | Mostly black: 76 of 768 world programs still skip their draws |
| ❌ | Playable | 0.45–0.46 fps and a black picture: no real gameplay |

### Shader translation

| Programs | Translated | Total | Progress |
|---|---|---|---|
| Vertex + pixel | 13,146 | 14,234 | `█████████░ 92%` |
| Compute | 2,404 | 2,582 | `█████████░ 93%` |
| Mesh | 8,396 | 8,713 | `██████████ 96%` |
| **All programs** | **23,946** | **26,084** | `█████████░ 92%` |
| World set, translate → SPIR-V → RADV pipeline | 692 | 768 | `█████████░ 90%` |

*Full archive: 26,084 programs, `translate-main.txt`, 4e7e6a3 (PR #704), 24 Nov 2026. World set: the 751 programs the world actually uses plus the 52-program BVH capture (768 once duplicate containers are skipped), each run through translation, SPIR-V validation and a real RADV pipeline (hub/main `c888a93`, 24 Nov 2026; also the report file `report-20261124-165507.tsv`) — every program that translates also validates and builds a pipeline, so the whole gap is translation. The 555 programs that are neither vertex/pixel, compute nor mesh are not emitted yet and count as untranslated.*

### Top remaining shader failures

| Failures | Reason |
|---|---|
| 716 | a descriptor's address or format read as data |
| 298 | a lane mask read as data |
| 195 | an address in an SGPR that is no root parameter |
| 166 | a packed input half no element holds |
| 88 | an SCC the emitter did not compute |
| 73 | an address not from a root parameter |
| 61 | an opaque lane mask |
| 48 | a cross-lane operation outside compute |

### Performance

| Measurement | RX 9070 XT, Linux / Wine (24 Nov 2026) |
|---|---|
| Menus | 30 fps |
| World | 0.45–0.46 fps |
| Title-memory copies | 628–864 ms/frame |
| Write-watch checks | 459–573 ms/frame |

### Tested hardware

| GPU | OS / driver | Furthest stage |
|---|---|---|
| RX 9070 XT (RDNA4) | CachyOS, Mesa 26.3.1 RADV; GE-Proton11-14 with vkd3d-proton master | World loads and plays audio, picture mostly black; 692 of 768 world programs build a RADV pipeline |

<!-- state:end -->

## What you need

- **Your own copy of the title.** None is included, and none will be linked or shared.
- A GPU with Direct3D 12 and shader model 6.6 (mesh shaders for the world), on Windows 10/11 x64
  or Linux with Wine/Proton.
- Rust (the version is pinned in `rust-toolchain.toml`) if you build from source.

## Quick start: Windows

1. Install [Rust](https://rustup.rs) and the Visual Studio 2022 Build Tools with "Desktop
   development with C++".
2. Get the code and make your config (paths must be outside the repo):
   ```powershell
   git clone https://github.com/pablogonz12/Scarlight
   cd Scarlight
   cargo xtask install-hooks                # asset-policy check on every commit
   copy victory.example.toml victory.toml   # then set your paths
   ```
3. Check the build and your paths (no game data needed for the tests):
   ```powershell
   cargo test --workspace
   cargo run -p victory -- config
   ```
4. Build and run:
   ```powershell
   cargo build --release -p victory
   target\release\victory.exe run <game dir>\<the title's exe> --execute
   ```

The title needs several GiB of free Windows commit at startup (Task Manager → Performance →
Memory → Committed). If `crashes.log` reports Windows error 1455, close other apps or raise the
paging file size.

## Quick start: Linux (Wine / GE-Proton)

1. Install Rust, then build a Windows binary from Linux:
   ```bash
   rustup target add x86_64-pc-windows-msvc
   cargo install cargo-xwin                   # once
   cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
   ```
   Add `--features diagnostics` for per-frame copy and watch statistics.
2. Install a GE-Proton build (tested: GE-Proton11-14) and run its `wine` binary directly, not the
   Proton script.
3. Put **vkd3d-proton master**'s `d3d12.dll` and `d3d12core.dll` next to `victory.exe` (the build
   output folder). Stock vkd3d-proton 3.1 crashes at world entry.
4. Set the environment and run:
   ```bash
   export WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3d11,d3d10core=n,b"
   export VKD3D_CONFIG=ignore_shared_fence
   export VICTORY_NO_WATCHDOG=1
   export VICTORY_GAME_DIR="$HOME/victory-data/game"
   /path/to/GE-Proton11-14/files/bin/wine \
     target/x86_64-pc-windows-msvc/release/victory.exe \
     run "$VICTORY_GAME_DIR/<the title's exe>" --execute
   ```

Free memory matters: anything that pins GPU memory (a local LLM server, say) pushes the world into
swap. More platform notes: [AMD-LINUX.md](AMD-LINUX.md).

## Controls

A gamepad works if one is connected. Otherwise the keyboard acts as a pad while the title's window
has focus:

| Keys | Pad |
|---|---|
| Enter, Backspace, X, Y | A, B, X, Y |
| Esc, Tab | Menu, View |
| Arrows | D-pad |
| W A S D | left stick |
| I J K L | right stick |
| Q / E | shoulders |
| Z / C | triggers |
| F / H | stick clicks |

## Troubleshooting

| Problem | Fix |
|---|---|
| Crash at world entry | Your `d3d12.dll`/`d3d12core.dll` are stock vkd3d-proton 3.1; use master's or GE-Proton11-14's instead |
| Out of memory, or the world crawls | Close apps that pin GPU memory; on Windows also check committed memory (error 1455) |
| World is black | Known, not fixed yet: shaders that do not translate have their draws skipped |
| The title waits for the GPU, then quits or restarts | `VICTORY_NO_WATCHDOG=1`; on 32-lane hosts (NVIDIA) also try `VICTORY_SKIP_WAVE64=mesh,compute` |
| `victory` says no paths, or exits at once | Run `victory config`: a path is unset, wrong, or inside the repo; set `VICTORY_GAME_DIR` |

Logs: `VICTORY_LOG=victory_loader=trace,info`. Crashes and host GPU failures are appended to
`crashes.log` in the data folder (default `%LOCALAPPDATA%\victory\data`). Diagnostics variables
and commands: [AMD-LINUX.md](AMD-LINUX.md#diagnostics).

## Contributing

Pull requests go to `main` only. CI runs clippy and the workspace tests on Windows, and CodeRabbit
reviews each PR. If you are merging someone's upstream fix, keep their commits and their credit:
[INTEGRATION.md](INTEGRATION.md) lists every merged pull request with its author, so it can be
sent upstream. Code that also helps Windows or other GPUs should go to
[Marvisak/VIctory](https://github.com/Marvisak/VIctory) too. Internal crate, binary and
environment-variable names stay `victory*` so upstream fixes keep merging. Never commit game files;
the asset-policy hook checks it (`docs/asset-policy.md`).

## License and credits

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_SKIP_WAVE64=mesh,compute` | omit draws and dispatches whose pipeline has a program of a listed stage written for 64-lane waves (on hosts with 32-lane waves, such as NVIDIA's, these can hang the GPU) |
| `VICTORY_PIPELINE_WAIT_MS=<n>` | milliseconds each title frame may spend waiting for pipelines being made before a draw or dispatch whose pipeline is not ready is dropped (default 500; 0 never waits, so first uses are dropped until their pipeline is made) — any build, not just this one |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
