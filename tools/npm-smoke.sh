#!/usr/bin/env bash
# Install the npm tarballs into a throwaway prefix and exercise them the way a user would.
#   tools/npm-smoke.sh <dir with .tgz files> <platform, e.g. linux-x64> [port]
# Checks: postinstall put the native binary at bin/aaf; aaf --version; start/status/stop on a
# throwaway home; reinstall with --ignore-scripts and check the Node launcher (exit codes, SIGTERM).
set -euo pipefail
dir=$(cd "$1" && pwd); plat=$2; port=${3:-18611}
shopt -s nullglob; mains=("$dir"/aa-forum-[0-9]*.tgz); shopt -u nullglob
[ ${#mains[@]} = 1 ] || { echo "expected exactly one aa-forum-<version>.tgz in $dir, found ${#mains[@]}" >&2; exit 1; }
main_tgz=${mains[0]}
ver=$(basename "$main_tgz" .tgz); ver=${ver#aa-forum-}
plat_tgz="$dir/aa-forum-$plat-$ver.tgz"
work=$(mktemp -d); prefix="$work/prefix"; home="$work/home"
trap 'aaf --home "$home" stop --force >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
aaf() { "$prefix/bin/aaf" "$@"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# The platform package first: the main package's optionalDependencies point at the registry,
# which does not have this version yet; npm skips that and finds the local one.
npm i -g --no-audit --no-fund --prefix "$prefix" "$plat_tgz" >"$work/npm.log" 2>&1 || { cat "$work/npm.log"; fail "installing $plat_tgz"; }
npm i -g --no-audit --no-fund --prefix "$prefix" "$main_tgz" >"$work/npm.log" 2>&1 || { cat "$work/npm.log"; fail "installing $main_tgz"; }
real=$(node -e "console.log(require('fs').realpathSync('$prefix/bin/aaf'))")
head -c 2 "$real" | grep -q '#!' && fail "bin/aaf is still the Node launcher after postinstall"
out=$(aaf --version); [[ $out == *"$ver"* ]] || fail "aaf --version is not $ver ($out)"
echo "ok: native binary at $real, version $ver"

aaf --home "$home" start --listen "127.0.0.1:$port" --timeout 60s
# Capture first: `status | grep -q` can cut the pipe while status is still writing.
out=$(aaf --home "$home" status); [[ $out == running* ]] || fail "status after start: $out"
aaf --home "$home" stop
set +e; aaf --home "$home" status >/dev/null; rc=$?; set -e
[ "$rc" = 3 ] || fail "status after stop should exit 3, got $rc"
echo "ok: start/status/stop"

npm rm -g --prefix "$prefix" aa-forum >/dev/null
npm i -g --no-audit --no-fund --ignore-scripts --prefix "$prefix" "$main_tgz" >"$work/npm.log" 2>&1 || { cat "$work/npm.log"; fail "installing $main_tgz with --ignore-scripts"; }
head -c 2 "$(node -e "console.log(require('fs').realpathSync('$prefix/bin/aaf'))")" | grep -q '#!' || fail "expected the Node launcher with --ignore-scripts"
out=$(aaf --version); [[ $out == *"$ver"* ]] || fail "launcher: --version ($out)"
set +e; aaf --home "$home" status >/dev/null; rc=$?; set -e
[ "$rc" = 3 ] || fail "launcher should pass exit code 3 through, got $rc"
# Foreground `aaf up` through the launcher: SIGTERM to the launcher must stop aaf cleanly.
# Run the file itself (not the shell function) so $! is the launcher's pid.
"$prefix/bin/aaf" --home "$home" up --listen "127.0.0.1:$port" >"$work/up.log" 2>&1 & pid=$!
for _ in $(seq 1 100); do curl -sf "http://127.0.0.1:$port/api/v1/health" >/dev/null && break; sleep 0.2; done
curl -sf "http://127.0.0.1:$port/api/v1/health" >/dev/null || fail "launcher: up did not answer"
kill -TERM "$pid"; set +e; wait "$pid"; rc=$?; set -e
[ "$rc" = 0 ] || fail "launcher: SIGTERM should end with 0, got $rc ($(tail -3 "$work/up.log"))"
grep -q 'signal="SIGTERM"' "$work/up.log" || fail "launcher: aaf did not see SIGTERM"
echo "ok: Node launcher (exit codes, SIGTERM forwarding)"
echo "smoke test passed for $plat $ver"
