#!/usr/bin/env bash
# 本機 CI（不依賴 GitHub Actions）。
#   tools/ci-local.sh            只跑 macOS（本機；目前預設）
#   tools/ci-local.sh mac linux  macOS＋Linux arm64（Docker）
#   tools/ci-local.sh linux      只跑 Linux arm64
#   tools/ci-local.sh linux-amd64  Linux amd64（QEMU 模擬，較慢）
set -euo pipefail
cd "$(dirname "$0")/.."
if [ $# -eq 0 ]; then targets=(mac); else targets=("$@"); fi
IMAGE=rust:1-bookworm

web_gate() {
  # 前端：型別檢查＋i18n 對照完整（web/node_modules 不存在時略過並提示）
  if [ -d web/node_modules ]; then (cd web && npx tsc --noEmit -p .) && node tools/i18n-check.cjs >/dev/null; else echo "skip web gate (run npm ci in web/)"; fi
}

gate() { web_gate && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace; }

run_linux() {
  local plat=$1
  docker run --rm --platform "$plat" -v "$PWD":/src:ro -v aaf-cargo-$(echo $plat|tr / -):/usr/local/cargo/registry \
    -w /work "$IMAGE" bash -c 'cp -a /src/. /work && rm -rf /work/target && rustup component add clippy rustfmt >/dev/null 2>&1 && \
      cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace'
}

summary=""; rc=0
for t in "${targets[@]}"; do
  echo "=== $t ==="
  case $t in
    mac) if gate; then r=pass; else r=FAIL; rc=1; fi ;;
    linux) if run_linux linux/arm64; then r=pass; else r=FAIL; rc=1; fi ;;
    linux-amd64) if run_linux linux/amd64; then r=pass; else r=FAIL; rc=1; fi ;;
    *) echo "unknown target $t"; exit 2 ;;
  esac
  summary="$summary$t: $r\n"
done
echo "=== summary @ $(git rev-parse --short HEAD)$(git diff --quiet || echo ' (dirty)') ==="
printf "$summary"
exit $rc
