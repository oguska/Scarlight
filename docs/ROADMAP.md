# Roadmap

**Decision (25 Nov 2026): Scarlight is a combination, and stays one.** Proton/Wine does the OS side;
Scarlight talks to Mesa directly for the GPU side. The GPU side is taken over step by step, and a step
is kept only when the fixed-scene run shows it helps. No dates: each step names what it has to prove
before the next one starts.

Every number below comes from one measured run, named where it appears.

## The layers

| Layer | Who does it today | Direction |
|---|---|---|
| **The game's code** (x86-64, CPU) | runs as it is, native on the CPU | stays |
| **The OS side** — C/C++ runtime, files, threads, windowing, Steam/Deck | Proton (Wine) | stays |
| **Console system functions** — the console DLLs the title imports | Scarlight's HLE modules | stays, and grows with what titles ask for |
| **GPU** — command stream, shaders, memory, pipelines | Scarlight → Direct3D 12 → vkd3d-proton → Vulkan → Mesa | **moving to Scarlight → Vulkan → Mesa**, one measured step at a time |

The split is deliberate. Replacing Wine with a Linux host is the expensive half and the half with the
least measured gain (below), while the GPU side is where the frame is spent and where an open driver
gives us something no closed one does.

## Why the split is there: the measured frame

One profile run, `layers-perf-1` (`best-fx-578d459c.exe`, 603 s, world reached, 2.2 fps median over 14
samples, 5,131-5,154 draws/frame), splits a 446 ms frame like this:

| Part of the frame | ms per frame | Share |
|---|---|---|
| Copies to the card (161-172 MB/frame) | 79-90 | 18 % |
| Copy-back from the card (81-85 MB/frame) | 96-139 | 22-28 % |
| Shader-write compare (find 20 ms + put back 34 ms, 398-419 MB) | 53-55 | 12 % |
| Write-watch (2,353 asks, 10.7-10.8 GB checked) | 30-36 | 7 % |
| On the GPU (wall time) | 152-164 | 30-33 % |
| The title's own waits and our recording | the rest | — |

Three things follow, and they set the order of the steps:

- **Memory is the biggest cost, and it is ours**: copies, copy-back, compare and write-watch together
  are about 60 % of the frame, and the card is busy only 32-33 % of the time.
- **The GPU numbers are not the bottleneck**: the driver submits what it is given and waits.
- **A backend rewrite is bounded, and the bound is small.** The second recording is the whole of
  vkd3d-proton's D3D12 replay of commands we already recorded. Removing all of it is worth an
  estimated 60-120 ms/frame, 2.2 → 2.6-3.0 fps; the absolute ceiling, the whole submit-path CPU, is
  270 ms. That is why it comes after the memory work, not before it.
- **A host without Wine is not planned.** It is 8-13 months of work (`nowine-spike.md`), and no
  measurement yet separates Wine's own share of the frame, because the one profile run attached to a
  wrapper process and produced no samples. Until a measurement says otherwise, the OS side stays with
  Wine, which also brings Steam and the Deck along for free.

## Steps, in order

Each step is tried on the current build, measured on the fixed test scene (two runs per side against
its baseline, see [testing.md](testing.md)), and kept only if the picture holds and the numbers move.
A step that fails is written down and left.

1. **Zero-copy title memory, through vkd3d-proton.** Import the title's memory into Vulkan with
   `OpenExistingHeapFromAddress` instead of copying it every frame. This is first because it attacks
   the 60 % above without touching the backend, and because it is cheap to test: it either works on
   RADV or it does not.
2. **The storage read fix.** DirectStorage reads on a page that is write-watched are copied in rather
   than faulted; it removes a failure that stops the title seconds into the world.
3. **A backend seam.** One `GpuBackend` trait behind the host layer, with the Direct3D 12 path as the
   only implementation at first. No behaviour change by itself; it is what makes the next two steps
   possible without a fork.
4. **Copies and memory on Vulkan.** Buffers, images, staging and copy scheduling move to a Vulkan
   backend, one resource kind at a time, each compared against the Direct3D 12 path on the fixed
   scene.
5. **ISA to SPIR-V directly.** The console's shaders are translated to DXIL and then to SPIR-V today;
   with our own backend they can be emitted as SPIR-V directly, one translation less.
6. **vkd3d-proton becomes optional.** With the Vulkan backend measured and ahead, the launcher offers
   a backend choice (`--backend d3d12|vulkan`), and the Direct3D 12 path stays as the compatibility
   route.

The backend is a runtime choice, not a branch: one `main`, two backends, and the default decided by
the fixed scene.

## In parallel: picture, and the cost of a frame before any of this

- **Picture parity** on the fixed scene: faces, full-detail characters up close, walls, no black
  flashes, colour as the reference. Picture comes first throughout: a speed change that costs picture
  quality is not kept.
- **Pre-built pipelines, like the console.** A measured frame spent 457 ms of 602 ms waiting for
  pipelines that were still being made; with a warm library the same wait was 0.0 ms, and 1,922
  pipelines were built in 1.3 s. Every world pipeline built before it is needed, slow ones
  quarantined, is the single largest title-side win.
- **Cache keys that survive a rebuild** (today the cache is keyed by our executable's hash, so a
  rebuild throws away 751 translations), **Mesa cache sizing** (RADV's disk cache here held 1.2 GB in
  312,556 files against Mesa's 1 GiB default, so it evicts and recompiles), and **fewer copies per
  frame** (99.4 % of the 1,347 watched ranges in one frame were unchanged).
- **Compile checks across Mesa GPU families**: 751 world programs, 278 compiled in 21.2 s, small
  enough to run per driver family before anyone boots the title.
