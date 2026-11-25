#!/usr/bin/env bash
# package.sh: build the release tarball a ProtonUp-Qt-style installer can extract.
#
# Usage: tools/steam-compat/package.sh --vkd3d DIR --dxc DIR [options]
#   --vkd3d DIR      folder with vkd3d-proton's d3d12.dll and d3d12core.dll (x64)
#   --dxc DIR        folder with DXC's dxil.dll and dxcompiler.dll (x64)
#   --exe PATH       the built victory.exe
#                    (default: target/x86_64-pc-windows-msvc/release/victory.exe)
#   --build NAME     the build name the exe is installed as, files/victory-<NAME>.exe (default: default)
#   --version VER    version in the file name (default: latest git tag, else 0.0.0)
#   --out DIR        where to write the tarball (default: dist)
#   -h, --help       this text
#
# The result is dist/Scarlight-<version>.tar.gz whose single top folder is
# Scarlight/, holding compatibilitytool.vdf, toolmanifest.vdf, scarlight,
# scarlight.conf, files/ and the licence texts of everything redistributed in it.
#
# What it packs is what may be redistributed: Scarlight itself (GPL-3.0),
# vkd3d-proton (LGPL-2.1) and DXC (Apache-2.0 with LLVM exception, MIT) are all
# free to pass on with their notices; Microsoft's d3dcompiler_47.dll is not, so
# it is never packed - the entry script runs without it. dxgi/d3d11/d3d10core
# come from the user's own Proton, and Wine comes from their Proton too.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
VKD3D=""
DXC=""
EXE="$ROOT/target/x86_64-pc-windows-msvc/release/victory.exe"
BUILDNAME="default"
VERSION=""
OUT="$ROOT/dist"

usage() { sed -n '2,24p' "$0"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --vkd3d) VKD3D="$2"; shift ;;
    --dxc) DXC="$2"; shift ;;
    --exe) EXE="$2"; shift ;;
    --build) BUILDNAME="$2"; shift ;;
    --version) VERSION="$2"; shift ;;
    --out) OUT="$2"; shift ;;
    -h | --help) usage; exit 0 ;;
    *)
      echo "unknown option $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

[ -n "$VKD3D" ] || { echo "pass --vkd3d DIR (d3d12.dll, d3d12core.dll)" >&2; exit 1; }
[ -n "$DXC" ] || { echo "pass --dxc DIR (dxil.dll, dxcompiler.dll)" >&2; exit 1; }
[ -f "$EXE" ] || {
  echo "Scarlight build not found: $EXE" >&2
  echo "(cargo xwin build --release -p victory --target x86_64-pc-windows-msvc)" >&2
  exit 1
}
for f in d3d12.dll d3d12core.dll; do
  [ -f "$VKD3D/$f" ] || { echo "missing $VKD3D/$f" >&2; exit 1; }
done
for f in dxil.dll dxcompiler.dll; do
  [ -f "$DXC/$f" ] || { echo "missing $DXC/$f" >&2; exit 1; }
done

if [ -z "$VERSION" ]; then
  # Release tags only: an unreleased checkout packs as 0.0.0, not as some
  # other line's tag.
  VERSION="$(git -C "$ROOT" describe --tags --match 'v*' --abbrev=0 2>/dev/null || echo 0.0.0)"
fi
VERSION="${VERSION#v}"

WORK="$(mktemp -d)"
trap 'rm -rf -- "$WORK"' EXIT
STAGE="$WORK/Scarlight"
mkdir -p "$STAGE/files/licenses"

install -m 755 "$HERE/scarlight" "$STAGE/scarlight"
cat >"$STAGE/compatibilitytool.vdf" <<'VDF'
"compatibilitytools"
{
  "compat_tools"
  {
    "Scarlight"
    {
      "install_path" "."
      "display_name" "Scarlight (Xbox Series)"
      "from_oslist" "windows"
      "to_oslist" "linux"
    }
  }
}
VDF
cat >"$STAGE/toolmanifest.vdf" <<'VDF'
"manifest"
{
  "version" "2"
  "commandline" "/scarlight %verb%"
  "require_tool_appid" "4183110"
  "use_sessions" "1"
}
VDF
install -m 644 "$EXE" "$STAGE/files/victory-$BUILDNAME.exe"
install -m 644 "$VKD3D/d3d12.dll" "$VKD3D/d3d12core.dll" "$DXC/dxil.dll" "$DXC/dxcompiler.dll" "$STAGE/files/"

# The conf a new install starts from: every option documented, Proton left for
# the user (or the installer) to fill in.
cat >"$STAGE/scarlight.conf" <<'CONF'
# Scarlight compatibility tool settings. Edit, then press Play again.
# Full reference: docs/steam.md in the Scarlight repository.

# Which build to run: files/victory-<BUILD>.exe (default: the shipped one).
BUILD=default

# The Proton whose Wine Scarlight runs on: a folder name in this
# compatibilitytools.d, or a full path to a Proton folder.
PROTON=GE-Proton11-14-x86_64

# Microsoft's d3dcompiler_47.dll is optional and not shipped. If you have one
# you are licensed to use, put its path here; empty means use Wine's built-in.
D3DCOMPILER=

# Where the title profiles are read from (default: ~/.config/victory/titles).
PROFILE_DIR=

# Mesa shader cache for this game (in the game's prefix) and its size cap.
MESA_CACHE_MAX_SIZE=16G

# vkd3d-proton options.
VKD3D_CONFIG=ignore_shared_fence

# Extra environment for the run, space separated, e.g. VICTORY_LOG=debug
EXTRA_ENV=
CONF

cp "$ROOT/LICENSE" "$STAGE/LICENSE"
cp "$HERE/licenses/LGPL-2.1.txt" "$HERE/licenses/vkd3d-proton-LGPL-2.1.txt" \
  "$HERE/licenses/DXC-LICENSE-LLVM.txt" "$HERE/licenses/DXC-LICENSE-MS.txt" \
  "$STAGE/files/licenses/"
install -m 644 "$HERE/licenses/README.md" "$STAGE/files/licenses/README.md"

mkdir -p "$OUT"
TARBALL="$OUT/Scarlight-$VERSION.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner \
  --mtime="@$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || echo 0)" \
  -C "$WORK" -czf "$TARBALL" Scarlight

echo "$TARBALL"
echo "  Scarlight/: compatibilitytool.vdf, toolmanifest.vdf, scarlight, scarlight.conf, LICENSE, files/"
echo "  files/: victory-$BUILDNAME.exe, d3d12.dll, d3d12core.dll, dxil.dll, dxcompiler.dll, licenses/"
echo "Extract it into Steam's compatibilitytools.d and restart Steam."
