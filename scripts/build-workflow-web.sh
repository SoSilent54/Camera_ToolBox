#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
web_dir="$repo_root/crates/frontends/workflow-web/web"

cd "$web_dir"
if [[ -f package-lock.json ]]; then
  npm ci
else
  npm install
fi
npm run build

cd "$repo_root"
cargo build -p camera-toolbox-workflow-web --release
