#!/usr/bin/env bash
# Sibling of scripts/package-windows.ps1: one portable binary plus its checksum.
set -euo pipefail

if [ "$(uname -s)" != 'Darwin' ]; then
    echo 'Run this script on macOS with Rust and the Xcode command line tools installed.' >&2
    exit 1
fi

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$project_root"

# No check at the argument site: the case below is the single validation point,
# so an unsupported host detected from 'rustc -vV' also gets the friendly message.
target=${1:-$(rustc -vV | sed -n 's/^host: //p')}
case "$target" in
    aarch64-apple-darwin) architecture=arm64; machine=arm64 ;;
    x86_64-apple-darwin) architecture=x64; machine=x86_64 ;;
    *) echo "Unsupported target: $target. Use aarch64-apple-darwin or x86_64-apple-darwin." >&2; exit 1 ;;
esac

# jq is preinstalled on GitHub's macOS runners and is the only tool assumed here
# beyond Rust and the Xcode command line tools.
version=$(cargo metadata --locked --no-deps --format-version 1 |
    jq -r '.packages[] | select(.name == "codex-copilot") | .version')

# Isolate portable builds. There is no static-CRT question on macOS: libSystem
# is the only system library and it is always linked dynamically.
build_root=$project_root/target/portable
cargo build --locked --release --target "$target" --target-dir "$build_root"

# Nothing else checks the cross-built binary (a runner is one architecture), so
# assert the Mach-O architecture of what was just built matches the target.
executable=$build_root/$target/release/codex-copilot
built_machine=$(lipo -archs "$executable")
if [ "$built_machine" != "$machine" ]; then
    echo "Built $executable is $built_machine, expected $machine for $target." >&2
    exit 1
fi

artifact_name=codex-copilot-$version-macos-$architecture
dist_root=$project_root/dist
mkdir -p "$dist_root"
artifact=$dist_root/$artifact_name
cp -f "$executable" "$artifact"
chmod +x "$artifact"

# Same '<hex>  <filename>' line as the Windows script, so 'shasum -a 256 -c'
# verifies it from the directory holding the download.
hash=$(shasum -a 256 "$artifact" | cut -d ' ' -f 1)
printf '%s  %s\n' "$hash" "$artifact_name" > "$artifact.sha256"
echo "Portable executable: $artifact"
echo "SHA256: $hash"
