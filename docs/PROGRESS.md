# Progress

**100 % in every row means every part the title uses is implemented and verified on Mesa; it is the target, not a guarantee, and correctness is checked by runs, not counts.**

Measured on 25 Nov 2026, build 5fca579b, RX 9070 XT (RDNA4) / Mesa 26.3.1 RADV. Diagnostics builds. No other GPU has been tried, and every number below comes from that one card.

- Every percentage is a share of a **named set that was counted** on this machine: an import table, a shader archive, a frame of recorded work. None of them is a share of the title as a whole, and the sets overlap (a program in the shader archive is not a distinct piece of the port).
- **Implemented** is what our code does today; **verified** is how much of that a run has shown working. They are separate columns because a decoder can claim an opcode that no run has proved. An em dash means that column was not counted for that row.
- The counts are taken from files the user supplies; nothing from them is stored in this repository, and no title, program or asset name appears here.
- A set can be complete without being ours: every import of the title **resolves** (0 missing), but 427 of the 587 are Wine (Proton) and 72 belong to the title's own libraries. The System APIs area shows both numbers side by side, and only the 88 HLE ones are code this project wrote.

Regenerate with `tools/progress/collect.sh` (local, needs your own copy of the title) and `tools/progress/render.py`; both are idempotent and the JSON holds counts only.

## System APIs (HLE)

What the title's import table asks for and who answers it. The image imports 587 functions from 68 DLLs and **all 587 resolve: 0 missing**. Only 88 of them are written by this project (our HLE modules); 427 are passed to Wine, which implements them, and 72 come from libraries the title ships itself. That split is the honest reading of the bars below: resolved is complete, Scarlight's own HLE covers 15 % of the table.

| Set | Implemented | Verified | Progress | What is counted |
|---|---|---|---|---|
| Imports resolved | 587 / 587 | 587 / 587 | ████████████ 100 % | 0 missing in the last world run: every import of the image and of the title's 68 DLLs binds |
| … of which by Scarlight's HLE modules | 88 / 587 | 88 / 587 | ██░░░░░░░░░░ 15 % | the console DLLs and the kernel calls this project implements itself |
| … of which by Wine (Proton) | 427 / 587 | 427 / 587 | █████████░░░ 73 % | passed through to the host's own implementations; working, but not our code |
| … of which by the title's own libraries | 72 / 72 | 72 / 72 | ████████████ 100 % | shipped with the title and loaded as they are: satisfied, nothing for us to write |
| Kernel and API sets (43 DLLs) | 280 / 280 | 280 / 280 | ████████████ 100 % | 35 to our HLE, 245 to Wine; 0 missing |
| Console DLLs: memory, audio, input, GPU | 38 / 38 | 38 / 38 | ████████████ 100 % | xmem 12, acphal 9, xapu 5, chad 5, d3d12_xs 3, xg_xs 2, xcrdapi 1, gameinput 1, all in our HLE |
| C runtime and OS (msvcp140, vcruntime, ws2_32, winhttp, bcrypt, mfplat) | 187 / 187 | 187 / 187 | ████████████ 100 % | 11 to our HLE, 176 to Wine |
| HLE exports in the registry beyond this image's needs | 134 | 88 used | spare | extra console API surface, so another build of the title can bind without a new module |

- Resolved counts come from the import census of the title image and from the loader's bound line in a world run (`hle=88 host=427 title=72 missing=0`).
- The 427 Wine-resolved imports are host pass-throughs: they work, and this project does not implement them. No row here claims them as ours.
- Networking modules exist in our HLE but are switched off; console-only networking is not a target of this port.

## Shaders

Two sets are counted: the 751-program world set used for offline checks, and the title's whole shader archive (26,084 programs). The world set is the one every change is checked against, end to end; the archive is the size of the real job, and only its translation and emission are counted so far.

| Set | Implemented | Verified | Progress | What is counted |
|---|---|---|---|---|
| World set, translated to host shader | 751 / 751 | 751 / 751 | ████████████ 100 % | 0 refused, 0 undecodable |
| World set, translated + validated + real RADV pipeline | 749 / 751 | 749 / 751 | ███████████░ 99 % | 0 stop: 751 of 751 reached a real RADV pipeline in the 25 Nov 13:14 run |
| Whole archive, translated | 26,084 / 26,084 | — | ████████████ 100 % | 40.6 M IR instructions; 25,889 programs have every resource access traced |
| Whole archive, host shader emitted: all stages | 25,602 / 25,694 | — | ███████████░ 99 % | vertex + pixel, compute and mesh together; the per-stage rows below add up to this |
| Whole archive, host shader emitted: vertex + pixel | 14,164 / 14,247 | — | ███████████░ 99 % | 83 programs of this stage are not emitted; no pipeline validation has been run on the archive yet |
| Whole archive, host shader emitted: compute | 2,676 / 2,682 | — | ███████████░ 99 % | 6 programs of this stage are not emitted; no pipeline validation has been run on the archive yet |
| Whole archive, host shader emitted: mesh | 8,762 / 8,765 | — | ███████████░ 99 % | 3 programs of this stage are not emitted; no pipeline validation has been run on the archive yet |
| ISA: opcode-table entries the decoder names | 313 / 1,232 | — | ███░░░░░░░░░ 25 % | the table is the full RDNA2 opcode list (1,232 entries across 20 encodings); 313 are named in the decoder, and the title uses far fewer than that. A per-program opcode census is not taken yet, so this row is a source count, not a decode census |

- Emitted per stage is 25,602 of 25,694 programs the archive classifies as vertex/pixel, compute or mesh; the further 390 are programs the console structure puts in none of those stages.
- The stop reasons are our own diagnostic classes, counted in the translator's summary; the largest single class is "a scalar memory read as data".
- The verified column covers the 751-program world set only. The archive has no validated pipeline column yet: translate alone is not a run.

## GPU command stream

What the title records per frame and what reaches the card. The headline row measures one real world frame: the draws and dispatches it recorded, minus the ones dropped. The reasons listed below come from a separate reference run and are **not** an accounting of that frame — they are the reasons drops have been seen, with the count each reached in the run that showed it. Ray tracing is off on purpose, so most drops are one intended reason and not a gap in the port.

| Set | Implemented | Verified | Progress | What is counted |
|---|---|---|---|---|
| Recorded work that reaches the card | 4,309 / 4,688 | 4,309 / 4,688 | ███████████░ 92 % | the last world frame (3,000 draws or more) of the newest run that has one: the backend's own per-frame lines |
| D3D12 calls the title makes that our backend has | 99 / 128 | 99 / 128 | █████████░░░ 77 % | present in the backend's sources; the rest are enum and helper surfaces, not missing entry points |
| Drops: ray-tracing hit shading, left out | — | — | off | 631/frame in the reference run of 24 Nov, which is a different run from the headline frame; behind a switch until the trace path is correct |
| Drops: one stage of the ray-tracing structure builder | — | — | off | 346-361/frame in that same reference run; off with the same switch |
| Drops: one whole-wave compute step | — | — | 14-28/frame | 14-28/frame in that same reference run: a section step our wave simulation does not follow yet |
| Drops: compute and pixel programs while their pipeline is made | — | — | 1-4/frame | 1-4/frame in that same reference run: each named program appears once and then runs, which is the pipeline area's business |

- The reference run records 168 world frames at 2.04 fps; drop reasons are the backend's own log lines, summed per frame in that run.
- Nothing is dropped as "no host pipeline": every drop is a named, deliberate or known gap.

## Pipelines

A draw cannot run until its pipeline exists on the host. The world set can be built entirely offline, and the pre-bake list covers most of what the title asks for in a session; what is left is created while the title waits for it.

| Set | Implemented | Verified | Progress | What is counted |
|---|---|---|---|---|
| World set: pipelines that build offline | 749 / 751 | 749 / 751 | ███████████░ 99 % | 2 of 751 do not reach a pipeline in this run |
| Pipelines the title asks for, covered by the pre-bake list | 1,785 / 2,601 | — | ████████░░░░ 69 % | 2,252 keys known ahead from the title's own early list, 1,785 of them buildable from the saved library |
| Late creates per frame in a world run | ~38 | measured | late | 63,794 drops over 1,673 frames of one run; they are the 1-4/frame rows above |

- The pre-bake list is the union of the early-pipeline key list and the warm pipeline library the title saved itself; the gap is what a session creates on demand.
- One program takes 3.6 s to compile alone, so the pre-bake must not schedule it last.

## Features

Status words, not percentages: a feature is either shown working on this machine, partly working, or off. These are the rows that decide whether the port is playable, and none of them is finished.

| Feature | Implemented | Verified | Progress | What is counted |
|---|---|---|---|---|
| Lit, textured world | — | — | partly | the fixed test scene renders lit and textured at 1.5 fps |
| Shadows | — | — | partly | shadowed areas read too dark; cascade selection is not verified |
| Characters and faces | — | — | broken | some characters' faces fail to render; regressed again on 25 Nov |
| Ray tracing | — | — | off | the structure builder and trace path sit behind a switch; the fallback shading is what draws |
| Audio | — | — | works | menu and world audio through the console audio DLLs |
| Input | — | — | works | gamepad through the console input DLL |
| Saves | — | — | not yet verified | the save module exists; no run has written and reloaded a save |
| Streaming (DirectStorage) | — | — | partly | reads work through the host file path; the submitted-queue path is not used |
| Video playback | — | — | partly | the title's own codec library loads; no cutscene has been played end to end |
| Networking | — | — | off | not a target of this port |

- "Partly" means the feature appears in at least one frame and is wrong or incomplete in a way the picture notes name; "broken" means it regressed and is being fixed.

## Mesa reach (compiled offline, not run)

Whether the world shader set compiles for a GPU family's Mesa driver, without that GPU. Mesa 26.3.1's noop drm-shim fakes each chip, so RADV and ANV build every pipeline with that family's ISA and limits. All 751 world pipelines compile on **4 of 5 AMD families** (RDNA4, RDNA3, RDNA2, RDNA1); Vega compiles 669 and aborts on the 82 mesh-shader programs. Three Intel families were measured (dg2, mtl, bmg) and all but one or two programs compile on each. This is a compile result: only RDNA4 has actually been run.

| GPU families | Implemented | Verified | Progress | What is counted |
|---|---|---|---|---|
| AMD families whose whole world set compiles offline | 4 / 5 | — | ██████████░░ 80 % | RDNA4, RDNA3, RDNA2, RDNA1: 751/751 each through the noop shim (Mesa driver, no GPU) |
| Intel (ANV) families measured offline | 3 / 3 | — | ████████████ 100 % | dg2 750/751, mtl 750/751, bmg 751/751 through Mesa's Intel noop shim; one program fails on two of the three, and there is no Intel card here to run on |
| Vega (gfx900): world programs that compile offline | 669 / 751 | — | ███████████░ 89 % | 82 abort the driver: every mesh-shader program, and GFX9 has no mesh shader support in RADV |
| Families with a run, not just a compile | 1 / 8 | — | ██░░░░░░░░░░ 12 % | RDNA4 only: 5 AMD and 3 Intel families have compiled pipelines, none of them has been run |

- Sources: `family-matrix-20261125-134909-2134785.tsv` (AMD, five families + the real card as control) and `family-matrix-20261125-140636-2410746.tsv` (Intel, three shim devices, 750-751 of 751 each); the shim column for our own family matches the real-GPU column exactly, so the harness does not change the answer.
- Compiled offline is not run. On RDNA1 all 82 mesh programs create, yet RADV does not advertise `VK_EXT_mesh_shader` there, so a program that uses mesh, barycentric or ray query at draw time needs the game's own fallback path.
- Intel: ANV needs `libvulkan_intel.so` plus Mesa's Intel noop shim from the same build; both were built for this run. The one program that fails on dg2 and mtl is the same class, and nothing Intel has been run.

## Performance

Speed is measured at one fixed scene, the only figure compared between builds. The console target for this title is 30 fps; the port is at about one twentieth of it, and the card is idle for most of the frame because the CPU side is the cost.

| Measure | Implemented | Verified | Progress | What is counted |
|---|---|---|---|---|
| Fixed test scene vs the 30 fps console target | 1.6 / 30 | 1.6 / 30 | █░░░░░░░░░░░ 5 % | median 1.95 fps at the window scene in the last recorded row; 1.45-2.30 across the 25 Nov rows, 3.0 in one earlier build |
| Card busy share of the frame | 30 / 100 | 30 / 100 | ████░░░░░░░░ 30 % | 24-38 % by our own clock count; the rest is the title's CPU work and our copies |
| Time to the world | 133-218 s | measured | no target set | from launch to the first world frame; the pre-bake should cut it |

- fps is the median of samples at the `window` scene, compared only between rows with the same scene label; `world_fps` from other runs mixes cutscenes and is never published.
- The frame is split as: title CPU work, our copies, then the card. Our copies are 98-111 ms of a 515-540 ms frame in the measured world run.

## How each number is measured

- **System APIs (HLE)**: the title image's import table is scanned by our own census tool, which marks each function as answered by one of our HLE modules, passed through to Wine, exported by the title's own images, or missing; a world run prints the same split on its `bound` line, which is the verified column. Only the HLE column is code we wrote
- **Shaders**: `victory translate` over the whole shader archive for the translated and emitted counts, and over the 751-program world set with dxil-spirv, `spirv-val` and a real RADV pipeline for the verified column
- **GPU command stream**: the backend's per-frame log lines in a world run, summed by drop reason; the D3D12 calls are the title's call list against the presence of each name in the backend's sources
- **Pipelines**: the offline world-set run (cold and warm Mesa cache), the title's own saved pipeline keys, and the `without a host pipeline` lines of a world run
- **Features**: run verdicts and the picture notes; a feature is never called working without a frame that shows it
- **Mesa reach**: the five-family offline matrix through Mesa's amdgpu noop shim, plus the real card as control; a family counts only when all of its pipelines compile
- **Performance**: the fixed-scene runner's median fps and the launch-to-world time from the loader's own timestamps

Generated by `tools/progress/render.py` from `docs/progress.json`, which `tools/progress/collect.sh` writes from local sources; both are plain text and hold counts only.
