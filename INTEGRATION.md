# Integration branch

This branch is VIctory `main` plus open pull requests and fixes from other branches, each one tested together, aiming for a build that boots and plays as far as possible. Every PR is merged as is, so its commits keep their authors. Credit goes to the people below; please review and merge their PRs upstream.

Maintained by @pablogonz12. Tested on Linux (RX 9070 XT, RADV, GE-Proton / vkd3d-proton master).

## Merged pull requests

| PR | Author | Title | Why |
|---|---|---|---|
| [#177](https://github.com/Marvisak/VIctory/pull/177) | @Ortomanic | Shaders: key heap handles by element type (atomic on a float-read UAV) | valid SPIR-V for atomics on float-read images (Linux) |
| [#190](https://github.com/Marvisak/VIctory/pull/190) | @LaunchDoesntCode | GPU: build the indirect-argument clamp with our own writer (no shader compiler at run time) | indirect-argument clamp without a shader compiler (Wine) |
| [#439](https://github.com/Marvisak/VIctory/pull/439) | @r4tedx | Shaders: one console wave sharing LDS runs in the console flow on 32-lane hosts | +3 world shaders |
| [#453](https://github.com/Marvisak/VIctory/pull/453) | @AntoninMichelq | Shaders: pixel barycentrics through SV_Barycentrics (values and computed interpolation) | barycentrics: +2 world pixel shaders |
| [#454](https://github.com/Marvisak/VIctory/pull/454) | @ruvenesh | Loader: opt-in VICTORY_NO_WATCHDOG hook for the title's watchdog (build 1.0.2.1) | VICTORY_NO_WATCHDOG for long world runs |
| [#457](https://github.com/Marvisak/VIctory/pull/457) | @jeremygold02 | fix(shader): accept overwritten VGPR inputs in counted indexed writes | +1 world shader |
| [#493](https://github.com/Marvisak/VIctory/pull/493) | @hophaver | GPU: bind root SRVs as one-view tables bounded to their host copy | root SRVs as bounded tables (world GPU faults, #752) |
| [#539](https://github.com/Marvisak/VIctory/pull/539) | @SAitken91 | Shaders: 64-bit image atomics | 64-bit image atomics: +4 world pixel shaders |
| [#571](https://github.com/Marvisak/VIctory/pull/571) | @SAitken91 | Shaders: LDS byte and half accesses | LDS bytes and halves: +1 world shader |
| [#573](https://github.com/Marvisak/VIctory/pull/573) | @banbanzhige | Shaders: lower ds_bpermute_b32 | ds_bpermute: +2 world shaders |
| [#579](https://github.com/Marvisak/VIctory/pull/579) | @wbkepler | GPU: VICTORY_SKIP_WAVE64 diagnostics switch for wave64 programs | VICTORY_SKIP_WAVE64 diagnostics |
| [#638](https://github.com/Marvisak/VIctory/pull/638) | @claude-the title | GPU diagnostics: find what the world hang waits for, and look under a black frame | world-hang diagnostics |
| [#704](https://github.com/Marvisak/VIctory/pull/704) | @r4tedx (carries [#193](https://github.com/Marvisak/VIctory/pull/193), @n-romanovich) | Shaders: structured buffers of unknown stride | world shaders 684 -> 692 of 751, all valid SPIR-V; conflicts with our draw.rs/pipeline/host.rs/lower.rs changes resolved keeping both sides |
| [#393](https://github.com/Marvisak/VIctory/pull/393) | @kxiserino | GPU: page-granular refresh ranges, journal fast path | with #124, 2 world runs each on RX 9070 XT / Wine: copies 2431-2571 -> 1732-2036 ms/frame, write-watch checks 1338-1351 -> 829-942 ms/frame, no device removed |
| [#124](https://github.com/Marvisak/VIctory/pull/124) | @TG0000 | GPU: remember compared unchanged pages | see #393 |
| [#1237](https://github.com/Marvisak/VIctory/pull/1237) | @Claude (upstream) | Shaders: implicit-level samples in compute take their gradients from the quad | 50 implicit-level compute samples in the 751-shader world dump now take `sampleGrad` with gradients read from the quad, instead of a `sample`/`sampleBias` the DXIL validator refuses in cs (black-world producer compute, #439 + 1,658 -> 2,193 of 2,193 validated on Windows); offline count unchanged 712/751 (dxil-spirv and RADV accept the old form too), no shader changed status. Conflict in `lower.rs` resolved keeping our console lane helpers beside the new `quad_gradients` |
| [#383](https://github.com/Marvisak/VIctory/pull/383) | @xmvo | Shader: LDS bytes and halves, constant sample offsets, descriptor-data | buffer descriptors read by `s_buffer_load` and as data: +231 of 1,692 #798 programs, +4 world shaders (677 to 681 of 751). Conflicts resolved: main's LDS byte/half code kept, the PR's tests updated to current signatures |
| [#592](https://github.com/Marvisak/VIctory/pull/592) | @SAitken91 | Shaders: second rows of packed 16-bit pixel inputs | full-archive run: vertex+pixel 13,144 -> 13,271 (+127), "a packed input half no element holds" 166 -> 0; world set unchanged |
| [#683](https://github.com/Marvisak/VIctory/pull/683) | @Halfect | Shaders: zero unavailable mixed-precision FMA halves | full-archive run: vertex+pixel 13,144 -> 13,152 (+8), "the first half of an interpolation" 28 -> 20; world set unchanged |
| fork commit [61474f6](https://github.com/MartinIndra02/victory/commit/61474f6) | @Noname0202, on @MartinIndra02's branch (issue [#783](https://github.com/Marvisak/VIctory/issues/783), @rocobalt1) | GPU: `VirtualQuery` region fallback and partial-copy progress in `copy_mapped`/`write_mapped` | the copy fallback skips contiguous unmapped regions instead of failing page by page; not measured on its own |

## Our own fixes

| Fix | Upstream |
|---|---|
| Shaders: a table descriptor read only as numbers takes its raw view's values (816/820 of that class; world 681 -> 684/751) | not sent yet |
| GPU: no blending on integer render targets in host pipelines | [#806](https://github.com/Marvisak/VIctory/issues/806) |

## Known test failure

`emit::tests::indexed_move_initializes_an_input_before_its_first_use` (from #457) fails on this branch (`WrongStart` case); it fails with or without #383.

## Measured

751 shaders dumped from a run into the world, re-translated offline: `main` 666 translate; this branch 677, all valid SPIR-V under vkd3d-proton master.
