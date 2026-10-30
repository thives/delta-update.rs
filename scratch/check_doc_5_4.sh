#!/bin/sh
# Validate the seven shell blocks in docs/checkpoint.md §5.4 as an operator uses them.
# Blocks 1-4 read their operator-supplied values from stdin; blocks share one shell.
set -u

repo=$(cd -- "$(dirname -- "$0")/.." && pwd -P) || exit 1
doc=$repo/docs/checkpoint.md
bin=$repo/target/debug/ota-apply
gen=$repo/target/debug/ota-gen
key=$repo/target/debug/private.pem

# Never reuse a predictable directory: fixtures contain destructive cleanup and moves.
work=$(mktemp -d "${TMPDIR:-/tmp}/ota-doc-5-4.XXXXXXXXXX") || exit 1
trap 'chmod -R u+rwX -- "$work" 2>/dev/null; rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM
chmod 700 "$work" || exit 1


if [ "$(id -u)" = 0 ]; then
  # The isolated sentinel child maps the invoking user to namespace root, then drops all capabilities.
  if [ "${OTA_DOC_SENTINEL_CHILD-}" != 1 ] || ! grep -q '^CapEff:[[:space:]]*0000000000000000$' /proc/self/status; then
    echo 'Run this harness as an unprivileged user; permission checks are not meaningful as root.' >&2
    exit 1
  fi
fi
for tool in "$bin" "$gen" "$key"; do
  if [ ! -f "$tool" ]; then
    printf 'missing %s (build the binaries and supply a test signing key)\n' "$tool" >&2
    exit 1
  fi
done

blocks=$work/blocks
inputs=$work/inputs
mkdir -p "$blocks" "$inputs" "$work/gen" || exit 1
checks=0
bad=0
t() {
  label=$1
  shift
  checks=$((checks + 1))
  if ! "$@" >/dev/null 2>&1; then
    bad=$((bad + 1))
    printf '  FAIL %s\n' "$label"
  fi
}
say() { printf '\n== %s ==\n' "$1"; }

# Hide the host's /tmp in a private mount namespace before planting at the real old path.
if [ "${OTA_DOC_SENTINEL_CHILD-}" != 1 ]; then
  say 'old fixed path survives a full harness run in an isolated /tmp'
  if unshare -Urnm sh -c '
    mount -t tmpfs -o mode=1777 tmpfs /tmp || exit 1
    mkdir /tmp/otafix || exit 1
    printf "sentinel data\n" > /tmp/otafix/sentinel || exit 1
    printf "sentinel data\n" > /tmp/expected || exit 1
    OTA_DOC_SENTINEL_CHILD=1 TMPDIR=/tmp setpriv --bounding-set=-all --inh-caps=-all --ambient-caps=-all sh "$1" || exit 1
    cmp -s /tmp/expected /tmp/otafix/sentinel || exit 1
    test "$(find /tmp/otafix -mindepth 1 -maxdepth 1 | wc -l)" -eq 1
  ' sh "$repo/scratch/check_doc_5_4.sh" > "$work/sentinel-log" 2>&1; then
    t 'full harness preserves the old-path sentinel' true
  else
    cat "$work/sentinel-log" >&2
    t 'full harness preserves the old-path sentinel (requires user and mount namespaces)' false
  fi
fi

# The section's exact headings and shell fences define the extraction boundary.
awk -v dir="$blocks" '
  /^### 5\.4 / { insec = 1; next }
  /^## 6\. Failure table/ { insec = 0 }
  insec {
    if (/^```sh$/ && !fence) { fence = 1; n++; next }
    if (fence && /^```$/) { fence = 0; next }
    if (fence) print > (dir "/" n ".sh")
  }
  END { print n+0 > (dir "/count") }
' "$doc" || exit 1
count=$(cat "$blocks/count")
if [ "$count" != 7 ]; then
  printf 'expected exactly seven sh blocks in section 5.4, found %s\n' "$count" >&2
  exit 1
fi
for n in 1 2 3 4 5 6 7; do
  if [ ! -s "$blocks/$n.sh" ]; then
    printf 'empty or missing documented block %s\n' "$n" >&2
    exit 1
  fi
done
printf 'extracted seven documented blocks\n'

# Each argument is a block number and an input-file suffix separated by a colon.
# Sourcing with redirection retains shell variables and the working directory;
# no operator input is ever parsed as shell code.
cat > "$work/runner.sh" <<'RUNNER'
blocks=$1
inputs=$2
statuses=$3
shift 3
: > "$statuses"
for step do
  number=${step%%:*}
  input=${step#*:}
  . "$blocks/$number.sh" < "$inputs/$input"
  status=$?
  printf '%s %s\n' "$step" "$status" >> "$statuses"
done
RUNNER
: > "$inputs/empty"
run_blocks() {
  cwd=$1
  shift
  (cd -- "$cwd" && sh "$work/runner.sh" "$blocks" "$inputs" "$work/status" "$@") > "$work/out" 2>&1
}
context() { printf '%s\n%s\nYES\n' "$1" "$2" > "$inputs/context"; }
setup_whole() { printf '%s\n' "$1" > "$inputs/whole"; }
setup_entry() { printf '%s\n%s\n' "$1" "$2" > "$inputs/entry"; }
setup_canonical() { printf '%s\n' "$1" > "$inputs/canonical"; }

# Snapshot names, kinds, modes, links and regular-file contents without following links.
# Python's byte paths preserve embedded newlines and other unusual filename bytes.
snapshot() {
  python3 - "$1" > "$2" <<'PY'
import hashlib, os, stat, sys
root = os.fsencode(sys.argv[1])
def walk(path, rel=b''):
    for name in sorted(os.listdir(path)):
        p = os.path.join(path, name)
        r = os.path.join(rel, name)
        s = os.lstat(p)
        kind = stat.S_IFMT(s.st_mode)
        extra = b''
        if stat.S_ISLNK(s.st_mode):
            extra = os.readlink(p)
        elif stat.S_ISREG(s.st_mode):
            with open(p, 'rb') as f:
                extra = hashlib.sha256(f.read()).digest()
        print(repr((r, kind, stat.S_IMODE(s.st_mode), s.st_nlink, extra)))
        if stat.S_ISDIR(s.st_mode):
            walk(p, r)
walk(root)
PY
}
fixture() {
  f=$work/$1
  mkdir -p "$f/locks" "$f/run" || exit 1
  chmod 700 "$f/locks" || exit 1
  truncate -s 8388608 "$f/run/dev.img" || exit 1
}
plant_init() {
  mkdir -p "$f/run/.ota-checkpoint.staging.init" || exit 1
  printf 'stray contents\n' > "$f/run/.ota-checkpoint.staging.init/stray"
  printf 'keep contents\n' > "$f/run/.ota-checkpoint.staging.init/keep"
  chmod "${1:-700}" "$f/run/.ota-checkpoint.staging.init"
}
tid=0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20
head -c 4194304 /dev/urandom > "$work/gen/new.img" || exit 1
if ! "$gen" --full -b 4096 "$work/gen/new.img" "$work/gen/payload.bin" "$key" > "$work/gen/log" 2>&1; then
  cat "$work/gen/log" >&2
  echo 'payload generation failed' >&2
  exit 1
fi
run_apply() {
  (cd -- "$f/run" && "$bin" --development --lock-dir "$f/locks" --target-id "$tid" \
    --checkpoint "$1" "$work/gen/payload.bin" dev.img) > "$work/apply" 2>&1
  apply_status=$?
}

say 'whole initialization route and real refusal'
fixture whole
plant_init
run_apply ck
t 'apply refuses unexpected initialization entries' sh -c 'test "$1" -ne 0 && grep -q "holds entries other than" "$2"' sh "$apply_status" "$work/apply"
t 'refusal leaves no record' test ! -e "$f/run/ck"
context "$f/run/ck" "$work"
setup_whole init-aside
run_blocks "$work" 1:context 2:whole 5:empty 7:empty
t 'context identifies the run directory' grep -Fq "$f/run" "$work/out"
t 'whole directory moved' test -f "$f/run/init-aside/stray"
t 'other entry was preserved' test -f "$f/run/init-aside/keep"
t 'old initialization name is absent' test ! -e "$f/run/.ota-checkpoint.staging.init"
run_apply ck
t 'rerun applies after move' sh -c 'test "$1" -eq 0 && grep -q "update applied" "$2"' sh "$apply_status" "$work/apply"

say 'entry route, listing and repeated refusal'
fixture entry
plant_init
context ck "$f/run"
setup_entry stray stray-aside
run_blocks "$work" 1:context 3:entry 5:empty 6:empty 7:empty
t 'listing shows selected entry' grep -q stray "$work/out"
t 'selected entry moved' test -f "$f/run/stray-aside"
t 'other entry remains' test -f "$f/run/.ota-checkpoint.staging.init/keep"
run_apply ck
t 'another entry still causes refusal' sh -c 'test "$1" -ne 0 && grep -q "holds entries other than" "$2"' sh "$apply_status" "$work/apply"

say 'canonical route and real refusal'
fixture canonical
mkdir -p "$f/run/.ota-checkpoint.staging"
printf 'canonical contents\n' > "$f/run/.ota-checkpoint.staging/keep"
chmod 755 "$f/run/.ota-checkpoint.staging"
run_apply ck
t 'apply refuses public staging directory' sh -c 'test "$1" -ne 0 && grep -q "staging directory must be owned by the updater and private" "$2"' sh "$apply_status" "$work/apply"
context ck "$f/run"
setup_canonical canonical-aside
run_blocks "$work" 1:context 4:canonical 5:empty 7:empty
t 'canonical directory moved with its contents' test -f "$f/run/canonical-aside/keep"
t 'canonical source is absent' test ! -e "$f/run/.ota-checkpoint.staging"

say 'failed context cannot reuse an earlier successful context'
fixture stale
plant_init
context ck "$f/run"
printf '%s\n%s\nYES\n' ck "$f/missing" > "$inputs/bad-context"
setup_whole stale-aside
run_blocks "$work" 1:context
t 'first context succeeds and identifies the correct directory' grep -Fq "$f/run" "$work/out"
snapshot "$f/run" "$work/before" || exit 1
run_blocks "$work" 1:context 1:bad-context 2:whole 5:empty 6:empty 7:empty
snapshot "$f/run" "$work/after" || exit 1
t 'second context reports failure' grep -Eq '^1:bad-context [1-9][0-9]*$' "$work/status"
t 'failed second context leaves entries unchanged' cmp -s "$work/before" "$work/after"
t 'failed second context does not report a move' sh -c '! grep -q "moved aside:" "$1"' sh "$work/out"
t 'inspection and move blocks reject the invalid context' sh -c 'for n in 5 6 7; do grep -Eq "^$n:empty [1-9][0-9]*$" "$1" || exit 1; done' sh "$work/status"
printf '%s\n' ck > "$inputs/incomplete-context"
run_blocks "$work" 1:context 1:incomplete-context 2:whole 7:empty
snapshot "$f/run" "$work/after" || exit 1
t 'incomplete context input clears earlier state' cmp -s "$work/before" "$work/after"
printf '%s\n%s\nNO\n' ck "$f/run" > "$inputs/unconfirmed-context"
run_blocks "$work" 1:context 1:unconfirmed-context 2:whole 7:empty
snapshot "$f/run" "$work/after" || exit 1
t 'unconfirmed context cannot enable a move' cmp -s "$work/before" "$work/after"
context ck relative
run_blocks "$work" 1:context 2:whole 7:empty
snapshot "$f/run" "$work/after" || exit 1
t 'relative original working directory is refused' cmp -s "$work/before" "$work/after"
context ck "$f/run"
run_blocks "$work" 1:context 2:whole 2:empty 7:empty
snapshot "$f/run" "$work/after" || exit 1
t 'failed setup clears prior source and destination' cmp -s "$work/before" "$work/after"
t 'failed setup reports failure' grep -Eq '^2:empty [1-9][0-9]*$' "$work/status"

say 'CDPATH decoy and trailing slash dot'
fixture cdpath
plant_init
mkdir -p "$f/decoy/run" "$f/run/sub"
# The normal relative checkpoint has an equally named CDPATH decoy.
context 'sub/ck' "$f/run"
setup_whole cdpath-aside
mkdir -p "$f/run/sub/.ota-checkpoint.staging.init" "$f/decoy/sub/.ota-checkpoint.staging.init"
printf real > "$f/run/sub/.ota-checkpoint.staging.init/stray"
printf decoy > "$f/decoy/sub/.ota-checkpoint.staging.init/stray"
CDPATH="$f/decoy"; export CDPATH
run_blocks "$work" 1:context 2:whole 7:empty
unset CDPATH
t 'CDPATH cannot redirect the move' test -f "$f/run/sub/cdpath-aside/stray"
t 'CDPATH decoy remains untouched' test -f "$f/decoy/sub/.ota-checkpoint.staging.init/stray"
# The supplied working directory can end in /.; the retained parent must
# still be the physical directory, not a different path or a blank value.
context ck "$f/run/sub/."
run_blocks "$work" 1:context
t 'working directory ending in slash dot resolves to physical parent' grep -Fq "$f/run/sub" "$work/out"
# FileCheckpoint treats sub/. as the record sub, whose parent is run.
context 'sub/.' "$f/run"
setup_whole dot-aside
snapshot "$f/decoy" "$work/before" || exit 1
run_blocks "$work" 1:context 2:whole 7:empty
snapshot "$f/decoy" "$work/after" || exit 1
t 'trailing slash dot identifies record sub' grep -q 'record name:      sub' "$work/out"
t 'trailing slash dot selects the record parent' test -f "$f/run/dot-aside/stray"
t 'trailing slash dot leaves decoy unchanged' cmp -s "$work/before" "$work/after"

say 'newline parent is refused without modifying entries'
fixture newline
newline_parent="$f/run/line
"
mkdir -p "$newline_parent/.ota-checkpoint.staging.init"
printf evidence > "$newline_parent/.ota-checkpoint.staging.init/stray"
# The input is one line, but physical resolution through this symlink reaches
# a parent ending in a newline, which command substitution would otherwise lose.
ln -s "$newline_parent" "$f/run/alias"
context 'alias/ck' "$f/run"
setup_whole newline-aside
snapshot "$f/run" "$work/before" || exit 1
run_blocks "$work" 1:context 2:whole 7:empty
snapshot "$f/run" "$work/after" || exit 1
t 'newline parent is refused by the context block' grep -Eq '^1:context [1-9][0-9]*$' "$work/status"
t 'newline parent leaves all entries unchanged' cmp -s "$work/before" "$work/after"
t 'newline parent does not report a move' sh -c '! grep -q "moved aside:" "$1"' sh "$work/out"

say 'quotes and shell metacharacters are data'
fixture metachar
special="dir 'quoted' \$(touch PWNED); &"
mkdir -p "$f/run/$special/.ota-checkpoint.staging.init"
printf evidence > "$f/run/$special/.ota-checkpoint.staging.init/stray"
context "$special/ck" "$f/run"
setup_whole "aside 'quoted' \$(touch PWNED); &"
run_blocks "$work" 1:context 2:whole 7:empty
t 'metacharacter destination receives the source' test -f "$f/run/$special/aside 'quoted' \$(touch PWNED); &/stray"
t 'operator input was not evaluated as shell code' test ! -e "$f/run/PWNED"
t 'operator input did not execute in the parent' test ! -e "$f/run/$special/PWNED"
entry_name="entry 'quote' \$(touch PWNED); &"
mkdir "$f/run/$special/.ota-checkpoint.staging.init"
printf evidence > "$f/run/$special/.ota-checkpoint.staging.init/$entry_name"
setup_entry "$entry_name" 'entry aside'
run_blocks "$work" 1:context 3:entry 7:empty
t 'entry name is literal data' test -f "$f/run/$special/entry aside"
t 'entry metacharacters were not executed' test ! -e "$f/run/$special/PWNED"

say 'existing destinations refuse without changing either entry'
for kind in file dir dangling; do
  fixture "dest-$kind"
  plant_init
  case $kind in
    file) printf 'destination\n' > "$f/run/occupied" ;;
    dir) mkdir "$f/run/occupied"; printf 'destination\n' > "$f/run/occupied/inside" ;;
    dangling) ln -s missing "$f/run/occupied" ;;
  esac
  context ck "$f/run"
  setup_whole occupied
  snapshot "$f/run" "$work/before" || exit 1
  run_blocks "$work" 1:context 2:whole 7:empty
  snapshot "$f/run" "$work/after" || exit 1
  t "$kind destination leaves the snapshot unchanged" cmp -s "$work/before" "$work/after"
  t "$kind destination is reported as refused" grep -q 'refused:' "$work/out"
done

say 'unsafe destination names refuse without changes'
for destination in . .. ck .OTA-Checkpoint.probe 'dir/name'; do
  fixture "invalid-$checks"
  plant_init
  context ck "$f/run"
  setup_whole "$destination"
  snapshot "$f/run" "$work/before" || exit 1
  run_blocks "$work" 1:context 2:whole 7:empty
  snapshot "$f/run" "$work/after" || exit 1
  t "destination $destination leaves entries unchanged" cmp -s "$work/before" "$work/after"
done

say 'permissions under an unprivileged user'
fixture permissions
plant_init 400
context ck "$f/run"
run_blocks "$work" 1:context 6:empty
t '0400 lists names' grep -q stray "$work/out"
t '0400 cannot show entry metadata' grep -q '?????????' "$work/out"
setup_entry stray aside
run_blocks "$work" 1:context 3:entry 5:empty
t '0400 cannot stat an entry' grep -Eq '^5:empty [1-9][0-9]*$' "$work/status"
chmod 300 "$f/run/.ota-checkpoint.staging.init"
run_blocks "$work" 1:context 6:empty
t '0300 cannot list names' sh -c '! grep -q stray "$1"' sh "$work/out"
t '0300 reports permission denied' grep -q 'Permission denied' "$work/out"
setup_whole permission-aside
run_blocks "$work" 1:context 2:whole 7:empty
t '0300 whole directory can move without widening permissions' test -d "$f/run/permission-aside"
t '0300 mode is preserved' sh -c 'test "$(stat -c %a -- "$1")" = 300' sh "$f/run/permission-aside"
chmod 700 "$f/run/permission-aside"

printf '\n%s checks, %s failed\n' "$checks" "$bad"
[ "$bad" -eq 0 ]
