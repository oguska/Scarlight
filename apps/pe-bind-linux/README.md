# pe-bind-linux

Offline feasibility probe for a Wine-free Linux host (spike, branch `spike/no-wine`). It answers three questions
with numbers instead of arguments, on Linux, with no Wine and no game run:

1. **Can the title image live at the address it asks for?** Reserves `SizeOfImage` at `ImageBase` with
   `mmap(MAP_FIXED_NOREPLACE)`, copies headers and sections in, applies every `IMAGE_REL_BASED_DIR64` relocation
   with the real delta, and binds all IAT slots.
2. **What would each import need?** Every import is classified against three tables — our HLE as it ships today
   (generated from the modules' own tables), a libc/pthread/mmap/futex/X11 equivalent, or "the Windows ABI itself"
   (MSVC C++ EH/RTTI/std, MSVC C runtime, SEH/unwind, media/net object models) — plus the title's own DLLs
   (verified against their export tables). Counts per DLL and per bucket, gaps listed with `-v`.
3. **How much of the Windows thread environment does the code read?** Sweeps the executable sections for `gs:`-
   relative operands and histograms the displacements (the TEB form).

The image is opened read-only. Nothing is written anywhere.

```sh
cargo build --target x86_64-unknown-linux-gnu -p pe-bind-linux
./target/x86_64-unknown-linux-gnu/debug/pe-bind-linux --image /path/to/title.exe [-v] [--list-imports]
python3 apps/pe-bind-linux/gen-hle-exports.py > apps/pe-bind-linux/data/hle-exports.tsv   # regenerate the HLE table
```

Results and the estimate they support: `xcl-bin/notes/nowine-spike.md`.
