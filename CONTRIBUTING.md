# Contributing to Scarlight

Thanks for helping. Scarlight is a GPL-3.0 port of [VIctory](https://github.com/Marvisak/VIctory):
it collects tested fixes from upstream's open pull requests, from forks and from other projects,
each merged with its author's credit, plus its own fixes. Contributions of any size are welcome,
and a test report is as useful as a patch.

## Where pull requests go

* Open pull requests against **`main` only**. Nobody pushes to `main` directly.
* Keep a pull request to one verifiable outcome. Say in the description what it changes and the
  evidence it is right: a log line, an offline shader count, frame timings.
* Before review, CI must be green and **CodeRabbit** must have reviewed it. Answer CodeRabbit's
  real findings; ignore style nits you disagree with, but say why.
* Small fixes: a branch, a pull request, done. Large ones: open an issue first so nobody
  duplicates work.

## Naming rule

* **Docs, commit messages, issue and pull request text never name the title or its developer.**
  Say "the title", "the game's files", "the user's copy". Not the game's name, not its studio's name,
  not their abbreviations, not in a comment or a commit either. This is not modesty: it is what keeps
  the repository up.
* **Internal names stay `victory*`**: crates (`victory-core`, `victory-gpu`, …), the `victory`
  binary, `victory.toml`, `VICTORY_*` environment variables. Renaming them breaks every upstream
  patch we still want to merge, so do not.

## Build

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it (`rustup toolchain install`)
with the `x86_64-pc-windows-msvc` target and the `rustfmt` and `clippy` components.

**On Linux** (cross-build with [cargo-xwin](https://github.com/rust-cross/cargo-xwin), no MSVC needed):

```sh
# once: clang is required by the xwin linker setup, cargo-xwin alone is not enough
cargo install cargo-xwin
cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
# add --features diagnostics for per-frame copy and write-watch statistics
```

Install your distribution's `clang` package as well (`clang` on Arch, `clang` on Debian/Ubuntu,
`clang` via `xcode-select` on macOS); `cargo xwin` drives it as the cross linker.

The Wine/Proton/vkd3d-proton setup that actually boots on RDNA4 is in [AMD-LINUX.md](AMD-LINUX.md).

**On Windows** (MSVC):

```sh
cargo build --release -p victory
```

## Test and lint exactly as CI does

CI (`.github/workflows/ci.yml`) runs on `windows-latest`, calls `rustup toolchain install` so the
pinned version and components are used, then runs these three steps. Run the same commands locally:

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
```

Warnings are errors, so a clean tree is required. These commands build for the **host** target, so on
Linux they do not compile the Windows-only test modules that CI runs; only a run on Windows matches
CI exactly. Cross-target checks such as
`cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings` catch
compilation breaks in that code, but they are not a substitute for the Windows test run.

`cargo test` includes xtask's `repository_passes_policy`, the asset-policy check, and the shader
tests in `crates/victory-shader/src/emit/diff_tests.rs`. Those tests compile inline HLSL cases with
DXC, and they run only when `VICTORY_CONSOLE_DXC` points at a console DXC binary; they need no game
files. Keep the tree formatted (`cargo fmt`);
CI does not check formatting, so do not reformat unrelated files.

## Asset policy

**Nothing from the game is ever stored here**, and nothing derived from it: no assets, no
executable bytes, no shader dumps, no extracted tables, no screenshots of game content, no hashes
of game data. Build identities (BLAKE3 of a user's executable, CodeView GUID), our own
byte-pattern signatures and our own synthetic fixtures are fine. The full list, including the one
documented exception, is in [docs/asset-policy.md](docs/asset-policy.md); read it before adding
anything binary.

## Crediting upstream authors

We credit by **merging the real commits**, not by copying code:

* Merge the upstream branch or pull request so its commits and author are kept (`git merge`, or a
  merge commit after resolving conflicts keeping both sides). Do not squash somebody else's work
  into ours, and do not retype their patch as an unnamed commit.
* Add a row to [INTEGRATION.md](INTEGRATION.md): pull request link, author, a neutral title, and
  why it matters (shader count, frame time, crash fixed).
* Send fixes that also help Windows or NVIDIA back upstream to Marvisak/VIctory. The point is to
  make their pull requests mergeable, not to fork forever.
* Disagreements about a merge go in the pull request, not in commit messages about people.

## Reporting without code

Use the issue forms: **Bug report** for a failure, **Test report** for a run on your hardware
(fps, crash or not, minutes run). Both remind you what never to attach. The last 50 lines of the
log around the failure are worth more than a screenshot.
