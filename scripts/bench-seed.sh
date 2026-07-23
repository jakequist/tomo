#!/usr/bin/env bash
# Seed benchmark harness (SEED-PERF Phase 0). Reproducible: seeded tree
# generator, localhost SSH, wall-clock via date +%s%N. Compares tomo (the
# built or installed binary) against rsync, optionally mutagen.
#
#   TOMO_BIN=target/release/tomo FILES=20000 ./scripts/bench-seed.sh
#
# Env: TOMO_BIN (default ~/.local/bin/tomo), FILES (default 20000),
#      MUTAGEN (path; skipped if unset/absent), KEEP=1 to keep the workdir.
set -uo pipefail

TOMO=${TOMO_BIN:-$HOME/.local/bin/tomo}
FILES=${FILES:-20000}
MUTAGEN=${MUTAGEN:-}
BASE=$(mktemp -d /tmp/tomo-bench-seed.XXXXXX)
trap '[ -z "${KEEP:-}" ] && rm -rf "$BASE"' EXIT

now_ns() { date +%s%N; }
ms() { echo $(( ($2 - $1) / 1000000 )); }

echo "== bench-seed: $FILES files, tomo=$($TOMO --version 2>/dev/null || echo '?'), $(date -u +%F)"

python3 - "$BASE/src" "$FILES" <<'EOF'
import os, random, sys
random.seed(42)
src, n = sys.argv[1], int(sys.argv[2])
for i in range(n):
    d = os.path.join(src, f"d{i%40:02d}", f"s{(i//40)%25:02d}")
    os.makedirs(d, exist_ok=True)
    size = random.choice([1024]*5 + [4096]*8 + [16384]*4 + [65536])
    with open(os.path.join(d, f"f{i:05d}.dat"), "wb") as f:
        f.write(random.randbytes(size))
EOF
echo "tree: $(find $BASE/src -type f | wc -l) files, $(du -sm $BASE/src | cut -f1) MB"

# --- rsync ------------------------------------------------------------------
DST_R=$BASE/dst-rsync; ssh localhost "mkdir -p $DST_R"
t0=$(now_ns); rsync -a -e ssh "$BASE/src/" "localhost:$DST_R/"; t1=$(now_ns)
echo "rsync seed: $(ms $t0 $t1) ms"

# --- tomo -------------------------------------------------------------------
DST_T=$BASE/dst-tomo; mkdir -p $DST_T
cp -a $BASE/src $BASE/src-tomo
( cd $BASE/src-tomo && $TOMO init ) >/dev/null 2>&1
( cd $DST_T && $TOMO init ) >/dev/null 2>&1
NF=$(find $BASE/src -type f | wc -l)
SB=$(find $BASE/src -type f -print0 | xargs -0 cat | wc -c)
t0=$(now_ns)
( cd $BASE/src-tomo && $TOMO sync -d "$(whoami)@localhost:$DST_T" ) >/dev/null 2>&1
until [ "$(find $DST_T -name .tomo -prune -o -type f -print 2>/dev/null | wc -l)" -eq "$NF" ]; do sleep 0.2; done
until [ "$(find $DST_T -name .tomo -prune -o -type f -print0 2>/dev/null | xargs -0 cat 2>/dev/null | wc -c)" -eq "$SB" ]; do sleep 0.2; done
t1=$(now_ns)
TOMO_MS=$(ms $t0 $t1)
echo "tomo seed: $TOMO_MS ms  ($(( TOMO_MS * 1000 / NF )) us/file)"
( cd $BASE/src-tomo && $TOMO stop ) >/dev/null 2>&1

# --- mutagen (optional) -----------------------------------------------------
if [ -n "$MUTAGEN" ] && [ -x "$MUTAGEN" ]; then
  DST_M=$BASE/dst-mutagen; ssh localhost "mkdir -p $DST_M"
  $MUTAGEN daemon start >/dev/null 2>&1
  t0=$(now_ns)
  $MUTAGEN sync create --name benchseed "$BASE/src" "localhost:$DST_M" >/dev/null 2>&1
  until [ "$(find $DST_M -type f 2>/dev/null | wc -l)" -eq "$NF" ]; do sleep 0.2; done
  t1=$(now_ns)
  echo "mutagen seed: $(ms $t0 $t1) ms"
  $MUTAGEN sync terminate benchseed >/dev/null 2>&1
  $MUTAGEN daemon stop >/dev/null 2>&1
fi
echo "== done"
