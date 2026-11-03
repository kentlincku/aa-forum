#!/usr/bin/env bash
# 打包 macOS arm64 版本：release 建置 → 簽章（有 Developer ID 用之，否則 ad-hoc）→ tar.gz＋SHA256。
# 用法：tools/release-mac.sh [輸出目錄]   版本號取自 Cargo.toml workspace.package.version
set -euo pipefail
cd "$(dirname "$0")/.."
out=${1:-dist}
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(uname -m)
name="aaf-${ver}-macos-${arch}"
cargo build --release -p aaf
mkdir -p "$out/$name"
cp target/release/aaf "$out/$name/"
cp README.md CHANGELOG.md docs/hooks.md "$out/$name/"
mkdir -p "$out/$name/examples" && cp -R examples/hooks "$out/$name/examples/"
id=$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' | head -1)
if [ -n "$id" ]; then
  codesign --force --options runtime --timestamp --sign "$id" "$out/$name/aaf"
  echo "signed with: $id"
else
  codesign --force --sign - "$out/$name/aaf"
  echo "ad-hoc signed (no Developer ID). First run: xattr -d com.apple.quarantine ./aaf"
fi
codesign --verify --verbose=1 "$out/$name/aaf"
"$out/$name/aaf" --version
tar -C "$out" -czf "$out/$name.tar.gz" "$name"
(cd "$out" && shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256")
ls -la "$out/$name.tar.gz"; cat "$out/$name.tar.gz.sha256"
