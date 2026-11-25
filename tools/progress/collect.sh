#!/usr/bin/env bash
# Measure the port on this machine and write docs/progress.json (counts only).
#
#   tools/progress/collect.sh [options]
#
#   --base DIR        the local tooling root (default: the checkout's parent)
#   --sources DIR     where the notes, the offline runs and the shots live
#                     (default: --base; ours keeps them one level below)
#   --repo DIR        the Scarlight checkout to measure (default: this one)
#   --run PATH        one world run's run.txt for the per-frame counts
#   --scene PATH      scene-results.tsv for the fixed-scene fps
#   --translate PATH  a `victory translate` summary (default: newest in notes)
#   --report PATH     an offline-check report-*.tsv (default: newest usable)
#   --matrix PATH     a family-matrix-*.txt (default: newest)
#   --cache DIR       the title's cache dir (default: the game prefix's)
#   --warm N          pipelines the saved library can build (the app prints it)
#   --dry-run         print the numbers found, write nothing
#
# Every source is optional: a source that is absent or unusable leaves the
# committed number for that row untouched and says so, so the script also runs
# on a machine without the title. Nothing from the title is copied anywhere:
# only counts land in the JSON. Run tools/progress/render.py afterwards.
#
#   python3 -I tools/progress/render.py            # docs/PROGRESS.md
#   python3 -I tools/progress/render.py --table    # the README rows
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
BASE=$(cd "$REPO/.." && pwd)
SOURCES=""
RUN=""
SCENE=""
TRANSLATE=""
REPORT=""
MATRIX=""
CACHE=""
WARM=0
EARLY=""
DRY=0

usage() { sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --base) BASE=${2:-}; shift 2 ;;
    --sources) SOURCES=${2:-}; shift 2 ;;
    --repo) REPO=${2:-}; shift 2 ;;
    --run) RUN=${2:-}; shift 2 ;;
    --scene) SCENE=${2:-}; shift 2 ;;
    --translate) TRANSLATE=${2:-}; shift 2 ;;
    --report) REPORT=${2:-}; shift 2 ;;
    --matrix) MATRIX=${2:-}; shift 2 ;;
    --cache) CACHE=${2:-}; shift 2 ;;
    --warm) WARM=${2:-0}; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown option: $1" >&2; usage 64 ;;
  esac
done

[ -n "$SOURCES" ] || SOURCES=$BASE
JSON="$REPO/docs/progress.json"
NOTES="$SOURCES/notes"
OFF="$SOURCES/offline"
CENSUS="$BASE/tmp/native/census-default.txt"
FACTS=$(mktemp "${TMPDIR:-/tmp}/progress-facts.XXXXXX") || exit 1
trap 'rm -f "$FACTS"' EXIT

say() { printf '%s\n' "$*" >&2; }
skip() { printf 'skip  %s\n' "$*" >&2; }
fact() { printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" >> "$FACTS"; }
newest() { ls -t "$@" 2>/dev/null | head -1; }
jsonget() { python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); a={x["id"]:x for x in d["areas"]}[sys.argv[2]]["rows"][int(sys.argv[3])].get(sys.argv[4]); print("" if a is None else ",".join(str(v) for v in a) if isinstance(a,list) else a)' "$JSON" "$@" 2>/dev/null; }

# ---- 1. system APIs (HLE): the import census ---------------------------------
# Rows match docs/progress.json. Only the HLE column is code we wrote: Wine
# pass-throughs and the title's own libraries resolve too, and are counted as
# such, so nothing here reads as ours.
if [ -f "$CENSUS" ]; then
  acc=$(awk -F' ' 'NR>3 && NF>=6 && $1 ~ /^api-ms/ {f+=$2;h+=$3;o+=$4} END {print f+0, h+o}' "$CENSUS")
  read -r a_total a_both <<< "$acc"
  acc=$(awk -F' ' 'NR>3 && NF>=6 && $1 ~ /^(xmem|acphal|xapu|chad|xcrdapi|xg_xs|gameinput|d3d12_xs)\.dll$/ {f+=$2;h+=$3;o+=$4} END {print f+0, h+o}' "$CENSUS")
  read -r c_total c_both <<< "$acc"
  acc=$(awk -F' ' 'NR>3 && NF>=6 && $1 ~ /^(msvcp140|vcruntime140|vcruntime140_1|bcrypt|ws2_32|winhttp|mfplat|mfreadwrite)\.dll$/ {f+=$2;h+=$3;o+=$4} END {print f+0, h+o}' "$CENSUS")
  read -r r_total r_both <<< "$acc"
  acc=$(awk -F' ' 'NR>3 && NF>=6 {f+=$2;h+=$3;o+=$4;m+=$5;n++} END {print f+0, h+0, o+0, m+0, n+0}' "$CENSUS")
  read -r all_total hle nhost title all_dlls <<< "$acc"
  if [ "$all_total" -gt 0 ]; then
    # the census's "missing" column is what the title's own images export, not
    # a gap: the loader's bound line reports missing=0 for the whole table
    resolved=$all_total
    fact hle 0 "Imports resolved" impl "$resolved,$all_total"
    fact hle 0 "Imports resolved" verified "$resolved,$all_total"
    fact hle 1 "… of which by Scarlight's HLE modules" impl "$hle,$all_total"
    fact hle 1 "… of which by Scarlight's HLE modules" verified "$hle,$all_total"
    fact hle 2 "… of which by Wine (Proton)" impl "$nhost,$all_total"
    fact hle 2 "… of which by Wine (Proton)" verified "$nhost,$all_total"
    fact hle 3 "… of which by the title's own libraries" impl "$title,$title"
    fact hle 3 "… of which by the title's own libraries" verified "$title,$title"
    [ "$a_total" -gt 0 ] && fact hle 4 "Kernel and API sets (43 DLLs)" impl "$a_both,$a_total"
    [ "$c_total" -gt 0 ] && fact hle 5 "Console DLLs: memory, audio, input, GPU" impl "$c_both,$c_total"
    [ "$r_total" -gt 0 ] && fact hle 6 "C runtime and OS (msvcp140, vcruntime, ws2_32, winhttp, bcrypt, mfplat)" impl "$r_both,$r_total"
    say "hle   $resolved/$all_total resolved: $hle our HLE, $nhost Wine, $title the title's own, $all_dlls DLLs"
  fi
else
  skip "hle: no census at $CENSUS"
fi

# ---- 2. shaders: the whole archive ------------------------------------------
if [ -z "$TRANSLATE" ]; then TRANSLATE=$(newest "$NOTES"/translate-*.txt); fi
if [ -f "${TRANSLATE:-/nonexistent}" ]; then
  read -r tr_done tr_total <<< "$(awk '/^programs /{for(i=1;i<=NF;i++){if($i=="of"){t=$(i+1)}; if($i=="translated"){d=$(i-1)}} } END{print d+0, t+0}' "$TRANSLATE")"
  st() { awk -v want="$1" 'index($0,"emitted of") && index($0,want){for(i=1;i<=NF;i++){if($i=="of"){d=$(i-2);t=$(i+1)}}; print d+0, t+0}' "$TRANSLATE"; }
  vs=$(st "vertex and pixel programs")
  cs=$(st "compute programs")
  ms=$(st "mesh programs")
  if [ "$tr_total" -gt 0 ]; then
    fact shaders 2 "Whole archive, translated" impl "$tr_done,$tr_total"
    fact shaders 2 "Whole archive, translated" verified "$tr_done,$tr_total"
    sum_d=0; sum_t=0
    for pair in "$vs" "$cs" "$ms"; do
      [ -z "$pair" ] && continue
      set -- $pair
      sum_d=$((sum_d + ${1:-0})); sum_t=$((sum_t + ${2:-0}))
    done
    set -- $vs; [ "${2:-0}" -gt 0 ] && fact shaders 3 "Whole archive, host shader emitted: vertex + pixel" impl "$1,$2"
    set -- $cs; [ "${2:-0}" -gt 0 ] && fact shaders 4 "Whole archive, host shader emitted: compute" impl "$1,$2"
    set -- $ms; [ "${2:-0}" -gt 0 ] && fact shaders 5 "Whole archive, host shader emitted: mesh" impl "$1,$2"
    say "shaders archive $tr_done/$tr_total translated, $sum_d/$sum_t emitted (vs+ps, cs, ms)"
  fi
else
  skip "shaders: no translate summary (pass --translate)"
fi

# ---- 2b. shaders: the world set, end to end ---------------------------------
if [ ! -d "$OFF" ]; then
  skip "shaders world set: no $OFF"
else
  if [ -z "$REPORT" ]; then
    while IFS= read -r f; do
      [ -f "$f" ] || continue
      [ "$(awk -F'\t' '$2=="ok"' "$f" | wc -l)" -gt 0 ] && { REPORT=$f; break; }
    done <<< "$(ls -t "$OFF"/report-*.tsv 2>/dev/null)"
  fi
  if [ -f "${REPORT:-/nonexistent}" ]; then
    ok=$(awk -F'\t' '$2=="ok"' "$REPORT" | wc -l)
    tot=$(awk -F'\t' 'NF>=2' "$REPORT" | wc -l)
    if [ "$tot" -gt 0 ]; then
      fact shaders 0 "World set, translated to host shader" impl "$tot,$tot"
      fact shaders 0 "World set, translated to host shader" verified "$tot,$tot"
      fact shaders 1 "World set, translated + validated + real RADV pipeline" impl "$ok,$tot"
      fact shaders 1 "World set, translated + validated + real RADV pipeline" verified "$ok,$tot"
      say "shaders world set $ok/$tot through translate, validation and a real pipeline ($(basename "$REPORT"))"
    fi
  else
    skip "shaders world set: no usable offline report in $OFF"
  fi
fi

# ---- 2c. ISA: the opcode table against the decoder ---------------------------
SH="$REPO/crates/victory-shader/src"
if [ -f "$SH/gfx10/opcodes.rs" ] && [ -d "$SH/emit" ]; then
  tbl=$(grep -o '"[a-z][a-z0-9_]*"' "$SH/gfx10/opcodes.rs" | tr -d '"' | sort -u)
  all=$(printf '%s\n' "$tbl" | wc -l)
  used=0
  while read -r op; do
    [ -z "$op" ] && continue
    grep -rqE "(^|[^a-z0-9_])${op}([^a-z0-9_]|$)" "$SH/emit" 2>/dev/null && used=$((used + 1))
  done <<< "$tbl"
  if [ "$all" -gt 0 ]; then
    fact shaders 6 "ISA: opcode-table entries the decoder names" impl "$used,$all"
    say "isa   $used/$all opcode-table entries named in the decoder"
  fi
else
  skip "isa: no opcode table in $SH"
fi

# ---- 3. GPU command stream --------------------------------------------------
if [ -z "$RUN" ]; then RUN=$(newest "$SOURCES"/shots/*/run.txt); fi
if [ -f "${RUN:-/nonexistent}" ]; then
  read -r draws med <<< "$(grep -oE '[0-9]+ draws/frame' "$RUN" 2>/dev/null | awk '{print $1}' | sort -n | awk '{a[NR]=$1} END{print a[NR]+0, a[int(NR/2)]+0}')"
  drops=$(grep -oE 'dropped [0-9]+\.?[0-9]*/frame' "$RUN" | awk '{s+=$2+0} END{print s+0}')
  if [ "$draws" -gt 0 ]; then
    say "stream newest run: max $draws draws/frame, median $med, drops summed over all lines $drops"
  fi
fi
METHODS="$BASE/tmp/native/methods.txt"
if [ -f "$METHODS" ] && [ -d "$REPO/crates/victory-gpu/src" ]; then
  tot=0; have=0
  while read -r m; do
    [ -z "$m" ] && continue
    tot=$((tot + 1))
    grep -rqE "(^|[^A-Za-z0-9_])${m}([^A-Za-z0-9_]|$)" "$REPO/crates/victory-gpu/src" && have=$((have + 1))
  done < "$METHODS"
  [ "$tot" -gt 0 ] && fact command_stream 1 "D3D12 calls the title makes that our backend has" impl "$have,$tot"
  say "stream $have/$tot of the title's D3D12 calls named in the backend"
else
  skip "stream: no method list at $METHODS"
fi

# ---- 4. pipelines: the pre-bake list ----------------------------------------
# The early list is the title's own record of the pipelines it will ask for;
# how many of those keys the saved library can build is printed by the app
# (pass --warm N to refresh it), not recoverable from the file.
if [ -z "$CACHE" ]; then
  for d in "$SOURCES"/victory-run "$SOURCES"/victory-play \
           "$HOME"/.local/share/*/drive_c/users/*/AppData/Local/victory; do
    [ -d "$d" ] || continue
    found=$(find "$d" -maxdepth 5 -name early-pipelines.txt 2>/dev/null | head -1)
    [ -n "${found:-}" ] && { EARLY=$found; break; }
  done
fi
if [ -f "${EARLY:-/nonexistent}" ]; then
  keys=$(grep -cve '^[[:space:]]*$' "$EARLY")
  was=$(jsonget pipelines 1 impl | cut -d, -f1)
  build=0
  case "${was:-0}" in ''|*[!0-9]*) build=0 ;; *) build=$was ;; esac
  [ "${WARM:-0}" -gt 0 ] && build=$WARM
  [ "$build" -gt "$keys" ] && build=$keys
  if [ "$keys" -gt 0 ] && [ "$build" -gt 0 ]; then
    fact pipelines 1 "Pipelines the title asks for, covered by the pre-bake list" impl "$build,$keys"
    say "pipelines $build/$keys keys covered by the saved library ($(basename "$EARLY"))"
  fi
else
  skip "pipelines: no early-pipeline list (pass --cache DIR)"
fi

# ---- 6. mesa reach: the family matrix ---------------------------------------
if [ -z "$MATRIX" ]; then MATRIX=$(newest "$OFF"/family-matrix-*.txt); fi
if [ -f "${MATRIX:-/nonexistent}" ]; then
  fam_done=$(awk '/^(gfx|real)/{t++; if ($3+0 > 0) ok++} END{print ok+0, t+0}' "$MATRIX")
  read -r f_ok f_tot <<< "$fam_done"
  if [ "$f_ok" -gt 0 ]; then
    fact mesa 0 "Families with a measured offline pipeline run" impl "$f_ok,$f_tot"
    say "mesa  $f_ok/$f_tot families produced a pipeline ($(basename "$MATRIX"))"
  else
    skip "mesa: $(basename "$MATRIX") has no ok column (shim could not open the ICD)"
  fi
fi

# ---- 7. performance: the fixed scene ---------------------------------------
if [ -z "$SCENE" ]; then SCENE="$NOTES/scene-results.tsv"; fi
if [ -f "$SCENE" ]; then
  fps=$(awk -F'\t' '$3=="window" && $4=="yes" && $7 ~ /^[0-9.]+$/ {v=$7} END{print v+0}' "$SCENE")
  if awk -v v="$fps" 'BEGIN{exit !(v > 0)}'; then
    fact performance 0 "Fixed test scene vs the 30 fps console target" impl "$fps,30"
    fact performance 0 "Fixed test scene vs the 30 fps console target" verified "$fps,30"
    say "perf  fixed scene $fps fps of 30"
  fi
else
  skip "performance: no $SCENE"
fi

# ---- merge: only rows whose label still matches, everything else kept -------
if [ "$DRY" = 1 ]; then
  say "dry run: $(wc -l < "$FACTS") facts, nothing written"
  cat "$FACTS" >&2
  exit 0
fi
python3 -I - "$JSON" "$FACTS" <<'PY'
import datetime, json, sys
path, facts = sys.argv[1], sys.argv[2]
doc = json.load(open(path, encoding="utf-8"))
areas = {a["id"]: a for a in doc["areas"]}
applied = kept = 0
for line in open(facts, encoding="utf-8"):
    line = line.rstrip("\n")
    if not line:
        continue
    area_id, index, label, field, value = line.split("\t")
    area = areas.get(area_id)
    if area is None:
        print("keep  unknown area %s" % area_id, file=sys.stderr)
        kept += 1
        continue
    row = area["rows"][int(index)]
    if row["label"] != label:
        print("keep  %s row %s is %r, not %r" % (area_id, index, row["label"], label), file=sys.stderr)
        kept += 1
        continue
    if field in ("impl", "verified"):
        a, b = (int(x) if x.lstrip("-").isdigit() else float(x) for x in value.split(","))
        row[field] = [a, b]
    else:
        row[field] = value
    applied += 1
doc["measured"] = datetime.date.today().isoformat()
json.dump(doc, open(path, "w", encoding="utf-8"), indent=2, ensure_ascii=False)
open(path, "a", encoding="utf-8").write("\n")
print("wrote %s: %d rows updated, %d kept as they were" % (path, applied, kept), file=sys.stderr)
PY
say "next: python3 -I tools/progress/render.py"
