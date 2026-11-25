# Licences of what a Scarlight release ships

The tarball that `tools/steam-compat/package.sh` builds carries two libraries that
are not ours. Both are redistributable, and both travel with their notices:

| File | What it covers | Licence | Text |
|---|---|---|---|
| `vkd3d-proton-LGPL-2.1.txt`, `LGPL-2.1.txt` | vkd3d-proton's `d3d12.dll` and `d3d12core.dll` (Direct3D 12 on Vulkan) | LGPL-2.1-or-later | the project's own notice, then the full LGPL-2.1 |
| `DXC-LICENSE-LLVM.txt`, `DXC-LICENSE-MS.txt` | DXC's `dxil.dll` and `dxcompiler.dll` (the HLSL compiler) | Apache-2.0 with LLVM exception (University of Illinois/NCSA), plus MIT for Microsoft's parts | upstream's two files, unmodified |

- **vkd3d-proton** — <https://github.com/HansKristian-Work/vkd3d-proton>. The DLLs
  are redistributed unmodified, as freed by LGPL-2.1; the sources are the upstream
  repository at the same version, so they can be rebuilt or relinked there. We ship
  the DLLs beside Scarlight and never link them into it.
- **DXC** — <https://github.com/microsoft/DirectXShaderCompiler>. Releases are
  published under the licences above, which permit redistribution with the notices
  and do not restrict use in a free-software project.

**Microsoft's `d3dcompiler_47.dll` is not shipped** and is never put in a release:
its licence does not permit redistribution. It is optional — the entry script runs
on Wine's built-in compiler, and if you have a copy you are licensed to use, point
`D3DCOMPILER` in `scarlight.conf` at it (see [docs/steam.md](../../../docs/steam.md)).

Everything else in the tarball is Scarlight's own: GPL-3.0-or-later, see `LICENSE`.
