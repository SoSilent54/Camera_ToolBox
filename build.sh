#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: ./build.sh [profile]

Builds one camera-toolbox executable for the current native host with Local,
CV610, SSH-managed providers, and OpenCV calibration enabled together. Set
CAMERA_TOOLBOX_CALIBRATION=0 to build without the OpenCV dependency.

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

features=product-all
if [[ ${CAMERA_TOOLBOX_CALIBRATION:-1} == "0" ]]; then
    features=platform-all
fi

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
target_dir=${CARGO_TARGET_DIR:-"${project_root}/target"}

printf 'Building camera-toolbox: features=%s profile=%s target_dir=%s\n' \
    "$features" "$profile" "$target_dir"

CARGO_TARGET_DIR="$target_dir" cargo build \
    --manifest-path "${project_root}/Cargo.toml" \
    --package camera-toolbox \
    --bin camera-toolbox \
    --no-default-features \
    --features "$features" \
    --locked \
    "${profile_args[@]}"
