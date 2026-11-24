# AMD RDNA4 + Linux notes

VIctory for **AMD RDNA4 (RADV) on Linux under Wine/Proton**. It is the `integration` branch
(VIctory `main` + tested open PRs, credits in `INTEGRATION.md`) plus fixes and measurements made on
this platform. Fixes that also help Windows/NVIDIA go upstream (Marvisak/VIctory) and to
[JMAN730/victory](https://github.com/JMAN730/victory).

## Tested setup

- RX 9070 XT (RDNA4), Mesa RADV, CachyOS (kernel 7.x), 30 GB RAM.
- GE-Proton11-14 wine run directly (not through the Proton script).
- **vkd3d-proton master** (`d3d12.dll`, `d3d12core.dll` next to `victory.exe`). Stock vkd3d-proton 3.1
  crashes at world entry (its dxil-spirv makes invalid SPIR-V that crashes RADV); master or the DLLs from
  GE-Proton11-14 do not.
- `WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3d11,d3d10core=n,b"`, `VKD3D_CONFIG=ignore_shared_fence`,
  `VICTORY_NO_WATCHDOG=1`.
- Free memory matters: anything pinning GPU memory (local LLM servers, etc.) pushes the world into swap.

## Build (from Linux)

```
cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
# add --features diagnostics for per-frame copy/watch statistics
```

## Status on this machine

Menus at 30 fps; the world loads and plays audio but renders black (shaders that do not translate
make their draws skip). World frame rate is limited by title-memory copies (0.2-0.8 fps); see
VIctory #783 and #400.

## Command reference

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Run `victory translate` with `--validate` to check emitted shaders with dxil.dll's validator, or
with `--pipelines` to also create a host D3D12 pipeline per shader.

## Diagnostics

A release build has no debugging aids. Build them in with:

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

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
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |
