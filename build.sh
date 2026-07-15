#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: ./build.sh [profile]

Builds one camera-toolbox executable for the current native host with Local,
CV610, and SSH-managed providers enabled together. OS/architecture builds are
split by native GitHub Actions runners rather than by this script.

Profiles:
  debug    Cargo dev profile (default)
  release  Cargo release profile
EOF
}

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
    usage
    exit 0
fi

if (( $# > 1 )); then
    usage >&2
    exit 2
fi

profile=${1:-debug}

case "$profile" in
    debug)
        profile_args=()
        ;;
    release)
        profile_args=(--release)
        ;;
    *)
        printf 'error: unsupported profile %q\n' "$profile" >&2
        usage >&2
        exit 2
        ;;
esac

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
target_dir=${CARGO_TARGET_DIR:-"${project_root}/target"}

printf 'Building camera-toolbox: providers=all profile=%s target_dir=%s\n' \
    "$profile" "$target_dir"

CARGO_TARGET_DIR="$target_dir" cargo build \
    --manifest-path "${project_root}/Cargo.toml" \
    --package camera-toolbox \
    --bin camera-toolbox \
    --no-default-features \
    --features platform-all \
    --locked \
    "${profile_args[@]}"
