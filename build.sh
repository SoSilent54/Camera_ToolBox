#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: ./build.sh <platform> [profile]

Platforms:
  local    Local RAW workflow only
  cv610    Local RAW plus Hisilicon CV610 provider
  ssh      Local RAW plus SSH-managed provider

Profiles:
  debug    Cargo dev profile (default)
  release  Cargo release profile
EOF
}

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
    usage
    exit 0
fi

if (( $# < 1 || $# > 2 )); then
    usage >&2
    exit 2
fi

platform=$1
profile=${2:-debug}

case "$platform" in
    local|cv610|ssh)
        feature="platform-${platform}"
        ;;
    *)
        printf 'error: unsupported platform %q\n' "$platform" >&2
        usage >&2
        exit 2
        ;;
esac

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
target_dir="${project_root}/target/${platform}"

printf 'Building camera-toolbox: platform=%s profile=%s target_dir=%s\n' \
    "$platform" "$profile" "$target_dir"

CARGO_TARGET_DIR="$target_dir" cargo build \
    --manifest-path "${project_root}/Cargo.toml" \
    --package camera-toolbox \
    --bin camera-toolbox \
    --no-default-features \
    --features "$feature" \
    --locked \
    "${profile_args[@]}"
