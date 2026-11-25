# Scarlight becomes a compatibility tool you pick in Steam

**25 November 2026**

Scarlight is changing shape. Until today it was a binary you copied next to the title and started by
hand, around a development setup of scripts, a GPU test queue and a measurement lab. From today the
thing we build and ship is **one Steam compatibility tool**: install it once, restart Steam, choose
**Scarlight (Xbox Series)** under the game's *Properties → Compatibility*, press Play. That is the
whole setup.

This is a change of packaging and of priorities, not of code: the loader, the console API layer, the
command-stream decoder and the shader translator are the same code, and the same binary runs inside.

## What the new implementation is

- **One install command.** `tools/steam-compat/install.sh` copies the tool into Steam's
  `compatibilitytools.d` (Steam's own folder, or Flatpak Steam's if that is what you use) together
  with the build and the DLLs it needs.
- **It rides on a Proton you already have.** The tool finds your installed GE-Proton or Valve Proton
  and uses that Wine for the operating-system side. We do not ship or fork Proton; the Proton version
  is one line in `scarlight.conf`.
- **It does the wiring itself.** Per-game prefix, per-game Mesa shader cache with room for the title,
  the title profile the loader needs, the DLL overrides, and the launch verbs Steam expects
  (`waitforexitandrun`, `getcompatpath`, `waitforexit`).
- **Steam-native.** The game appears in your library, Play works, Steam's overlay and playtime work,
  and there is nothing to launch from a terminal.
- **Same split as before.** Proton (Wine + vkd3d-proton) carries the OS side; the GPU side stays ours
  and moves toward talking to Mesa more directly, step by step — see [docs/architecture.md](docs/architecture.md).

## What happens to the previous implementation

**Nothing is deleted.** The development setup of that era — the offline shader lab, the GPU run queue,
the performance ledger, the worker pipeline and everything measured with it — is preserved in full on:

- branch **`legacy/amd-hub`**
- tag **`legacy-amd-hub-2026-11-25`**

Both point at the last commit of that line (`ce65955`). `main` continues from exactly that commit, so
the two lines share their whole history up to this point: nothing was rewritten, squashed or
force-pushed, and any script you used from that branch still checks out and still builds.

`main` keeps the runtime — the part a player needs. The measurement scripts stay on the legacy branch
as a reference for how the numbers in this README were taken.

## What this changes for you

- If you already run the title by hand (`victory.exe run <exe> --execute`), **nothing breaks**. The
  tool is a wrapper around that same binary, so the manual route keeps working.
- If you only want the plain binary, the releases keep providing it; the tool is an addition.
- What it does **not** do is make the title faster by itself. The status table in the [README](README.md)
  is unchanged by this announcement: same picture, same loading time, same frame rate. The gains come
  from the work listed in the roadmap, and each one lands as its own measured change.

## Why the change

The old setup answered "how do we measure this title on this machine", and it answered it well. The
question that matters to anyone else is "how do I run it", and for a title this early the honest
answer has to be a Steam tool, because that is where such a layer is selected and where it has to
work: Steam hands us the prefix, the environment and the launch verbs, and whatever we build has to
survive that. Building against Steam's contract from now on keeps us from growing conveniences that
only work in our own scripts.

## Status and honesty

Unchanged from the README: the title is **not playable yet**. It boots, runs its menus, loads the
world and renders it lit and textured at a few frames per second on one AMD card under Mesa; faces,
some geometry and shadowed areas still need work, and loading is slow. Every number in the README was
measured on one machine (RX 9070 XT, Mesa RADV, Linux under Proton), not tested on NVIDIA hardware,
and reports from other hardware are welcome — that is what [issues](https://github.com/pablogonz12/Scarlight/issues)
are for.
