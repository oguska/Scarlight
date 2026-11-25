# Licences of what a Scarlight release ships

The tarball that `tools/steam-compat/package.sh` builds carries two libraries that are not ours. Both
travel with their notices, and `files/licenses/VERSIONS.txt` in the tarball names the exact versions
and where their source is.

| File | What it covers | Terms |
|---|---|---|
| `vkd3d-proton-LGPL-2.1.txt`, `LGPL-2.1.txt` | vkd3d-proton's `d3d12.dll` and `d3d12core.dll` (Direct3D 12 on Vulkan) | the project's own notice, then the full LGPL-2.1; LGPL-2.1-or-later |
| `DXC-LICENSE-MS.txt` | DXC's `dxil.dll` and `dxcompiler.dll` as released by Microsoft | Microsoft's licence terms for the DXC binaries, including the section that makes the distributable files redistributable with their notices |
| `DXC-LICENCE-MIT.txt` | DXC's own source | MIT |
| `DXC-LICENSE-LLVM.txt` | LLVM, which DXC is built on | University of Illinois/NCSA |
| `DXC-CLANG-LICENSE-Apache-2.0-LLVM.txt` | Clang, which compiled it | Apache-2.0 with LLVM exception |

- **vkd3d-proton** — <https://github.com/HansKristian-Work/vkd3d-proton>. The DLLs are redistributed
  unmodified, as LGPL-2.1 allows, and the release records the exact tag they came from. LGPL-2.1
  section 6 covers distributed object code: the source for that tag is at the URL in `VERSIONS.txt`,
  the release attaches the matching source archive, and the written offer in `VERSIONS.txt` holds for
  three years. They are shipped beside Scarlight, never linked into it.
- **DXC** — <https://github.com/microsoft/DirectXShaderCompiler>. The release carries all five notice
  files above, unmodified. Its licence terms are Microsoft's for the binaries — not MIT, and not a
  free-software licence — and they are the reason this project treats the two DLLs as optional:
  delete them from `files/` and the tool still runs, using a copy you supply yourself.

**Microsoft's `d3dcompiler_47.dll` is not shipped** and never goes into a release. It is optional — the
entry script runs on Wine's built-in compiler, and if you have a copy you are licensed to use, point
`D3DCOMPILER` in `scarlight.conf` at it (see [docs/steam.md](../../../docs/steam.md)).

Everything else in the tarball is Scarlight's own: GPL-3.0-or-later, see `LICENSE`.
