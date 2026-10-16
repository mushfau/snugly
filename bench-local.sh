#!/bin/bash
# snugly benchmark for macOS — runs entirely on this machine. Nothing is uploaded anywhere.
#
#   ./bench-local.sh SOURCE_DIR [OUTPUT_DIR]
#
# SOURCE_DIR  the folder to compress, e.g. ~/Pictures/Photos\ Library.photoslibrary/originals
# OUTPUT_DIR  defaults to ./bench-out (needs free space ≈ 2× the source)
#
# Compares zip -9, 7-Zip Ultra (if installed: `brew install sevenzip`) and snugly, then tests
# verification and an incremental update. Results go to OUTPUT_DIR/results.txt — sizes,
# times and memory only, no file names.

set -uo pipefail
[ $# -ge 1 ] || { echo "usage: $0 SOURCE_DIR [OUTPUT_DIR]"; exit 1; }
abs() { case "$1" in /*) echo "$1";; *) echo "$PWD/$1";; esac; }
SRC="$(abs "$1")"
OUT="${2:+$(abs "$2")}"
cd "$(dirname "$0")"
SNUGLY="$PWD/target/release/snugly"
OUT="${OUT:-$PWD/bench-out}"

[ -x "$SNUGLY" ] || { echo "Build first:  cargo build --release"; exit 1; }
if ! ls "$SRC" >/dev/null 2>&1; then
  echo "Can't read: $SRC"
  echo "If this is your Photos library, give Terminal access in System Settings → Privacy & Security → Full Disk Access, then reopen Terminal."
  exit 1
fi
mkdir -p "$OUT"
RES="$OUT/results.txt"
IN=$(( $(du -sk "$SRC" | cut -f1) * 1024 ))
AVAIL=$(( $(df -k "$OUT" | awk 'NR==2{print $4}') * 1024 ))
if [ "$AVAIL" -lt $(( IN * 2 )) ]; then
  echo "Not enough free space in $OUT: need about $(( IN * 2 / 1000000000 )) GB, have $(( AVAIL / 1000000000 )) GB."
  exit 1
fi

T=0; M=0
run() {  # run a command, capturing wall time (s) and peak memory (MB)
  /usr/bin/time -lp "$@" > /dev/null 2> "$OUT/.time"
  local rc=$?
  T=$(awk '/^real/{print $2}' "$OUT/.time")
  M=$(awk '/maximum resident set size/{printf "%d", $1/1048576}' "$OUT/.time")
  if [ $rc -ne 0 ]; then echo "FAILED (exit $rc):"; grep -v -E '^(real|user|sys) |^ +[0-9]+ ' "$OUT/.time" | tail -5; return $rc; fi
}
row() {  # label, bytes, seconds, mem
  awk -v l="$1" -v b="$2" -v t="$3" -v m="$4" -v i="$IN" \
    'BEGIN{printf "%-30s %10.1f MB  %6.1f%%  %8.1f s  %6d MB RAM\n", l, b/1e6, b*100/i, t, m}' | tee -a "$RES"
}

{
  echo "snugly local benchmark — $(date)"
  echo "machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null), $(( $(sysctl -n hw.memsize) / 1073741824 )) GB RAM"
  echo "source:  $(find "$SRC" -type f | wc -l | tr -d ' ') files, $(awk -v i=$IN 'BEGIN{printf "%.1f", i/1e6}') MB"
  echo
  printf "%-30s %13s  %7s  %10s  %13s\n" "tool" "size" "ratio" "time" "peak memory"
} | tee "$RES"

# 1. zip -9
rm -f "$OUT/a.zip"
echo "… zip -9" >&2
pushd "$SRC" > /dev/null
run zip -9 -r -q "$OUT/a.zip" . && ZIPOK=1 || ZIPOK=0
popd > /dev/null
[ "$ZIPOK" = 1 ] && row "zip -9" "$(stat -f%z "$OUT/a.zip")" "$T" "$M"

# 2. 7-Zip Ultra (solid LZMA2), if installed
SZ=$(command -v 7zz || command -v 7z || true)
if [ -n "$SZ" ]; then
  rm -f "$OUT/a.7z"
  echo "… 7-Zip Ultra" >&2
  run "$SZ" a -mx=9 -ms=on -bd -bso0 -bsp0 "$OUT/a.7z" "$SRC/." && row "7-Zip Ultra" "$(stat -f%z "$OUT/a.7z")" "$T" "$M"
else
  echo "(7-Zip not installed — 'brew install sevenzip' to include it)" | tee -a "$RES"
fi

# 3. snugly balanced
rm -f "$OUT/a.snug"
echo "… snugly balanced" >&2
run "$SNUGLY" c "$OUT/a.snug" "$SRC" -m balanced && row "snugly balanced" "$(stat -f%z "$OUT/a.snug")" "$T" "$M"
grep -E '^snapshot' "$OUT/.time" | sed 's/^/    /' | tee -a "$RES"

# 4. verify every file
echo "… snugly verify" >&2
run "$SNUGLY" t "$OUT/a.snug" && printf "%-30s %s\n" "snugly verify (all hashes)" "$T s, $M MB RAM — OK" | tee -a "$RES"

# 5. incremental update with nothing changed
echo "… snugly update (no changes)" >&2
BEFORE=$(stat -f%z "$OUT/a.snug")
run "$SNUGLY" c "$OUT/a.snug" "$SRC" -u --label "re-run" && \
  printf "%-30s %s\n" "snugly update, no changes" "$T s, +$(( ($(stat -f%z "$OUT/a.snug") - BEFORE) / 1000 )) KB" | tee -a "$RES"

# 6. snugly max (optional, slower): SNUGLY_MAX=1 ./bench-local.sh
if [ "${SNUGLY_MAX:-0}" = 1 ]; then
  rm -f "$OUT/max.snug"
  echo "… snugly max" >&2
  run "$SNUGLY" c "$OUT/max.snug" "$SRC" -m max && row "snugly max" "$(stat -f%z "$OUT/max.snug")" "$T" "$M"
fi

rm -f "$OUT/.time" "$OUT/a.zip" "$OUT/a.7z" "$OUT/max.snug"
echo
echo "Done. Results: $RES"
echo "The snugly archive is kept at $OUT/a.snug so you can try  $SNUGLY s / x / t  on it. Delete bench-out when finished."
