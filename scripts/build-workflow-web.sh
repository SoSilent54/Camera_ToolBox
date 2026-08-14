#!/usr/bin/env bash
set -euo pipefail

if [[ $# -gt 1 ]]; then
  printf 'usage: %s [debug|release]\n' "${BASH_SOURCE[0]}" >&2
  exit 2
fi

profile=${1:-release}
case "$profile" in
  debug)
    cargo_profile=()
    ;;
  release)
    cargo_profile=(--release)
    ;;
  *)
    printf 'error: unsupported profile %q\n' "$profile" >&2
    printf 'usage: %s [debug|release]\n' "${BASH_SOURCE[0]}" >&2
    exit 2
    ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
web_dir="$repo_root/crates/frontends/camera-toolbox-web/web"

cd "$web_dir"
if [[ -f package-lock.json ]]; then
  npm ci
else
  npm install
fi
npm run build

cd "$repo_root"
cargo build -p camera-toolbox-web "${cargo_profile[@]}"
