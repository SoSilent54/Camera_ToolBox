#!/usr/bin/env bash
set -euo pipefail

configure_windows_msvc_linker() {
    if [[ ${OS:-} != "Windows_NT" || -n ${CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER:-} ]]; then
        return
    fi
    if [[ -z ${VCToolsInstallDir:-} ]]; then
        printf 'error: VCToolsInstallDir is not set; run from an activated MSVC x64 environment\n' >&2
        exit 1
    fi
    if ! command -v cygpath >/dev/null 2>&1; then
        printf 'error: cygpath is required when build.sh runs on Windows\n' >&2
        exit 1
    fi

    local tools_dir linker_unix linker_windows
    tools_dir=$(cygpath --unix "$VCToolsInstallDir")
    linker_unix="${tools_dir%/}/bin/Hostx64/x64/link.exe"
    if [[ ! -f $linker_unix ]]; then
        printf 'error: MSVC linker does not exist: %s\n' "$linker_unix" >&2
        exit 1
    fi
    linker_windows=$(cygpath --windows "$linker_unix")
    export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="$linker_windows"
    printf 'Using MSVC linker: %s\n' "$linker_windows"
}

usage() {
    cat <<'EOF'
Usage: ./build.sh [profile]

Builds one camera-toolbox executable for the current native host with Local,
CV610, SSH-managed providers, and pinned OpenCV 5 calibration enabled together.
The verified native dependency is cached under .deps/opencv5 and its runtime is
copied beside the executable. Set CAMERA_TOOLBOX_CALIBRATION=0 to build without
the OpenCV dependency.

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
calibration_enabled=1
if [[ ${CAMERA_TOOLBOX_CALIBRATION:-1} == "0" ]]; then
    features=platform-all
    calibration_enabled=0
fi

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
target_dir=${CARGO_TARGET_DIR:-"${project_root}/target"}
export CARGO_TARGET_DIR="$target_dir"
configure_windows_msvc_linker

printf 'Building camera-toolbox: features=%s profile=%s target_dir=%s\n' \
    "$features" "$profile" "$target_dir"

cargo_args=(
    build
    --manifest-path "${project_root}/Cargo.toml"
    --package camera-toolbox
    --bin camera-toolbox
    --no-default-features
    --features "$features"
    --locked
    "${profile_args[@]}"
)

helper_args=(
    build
    --manifest-path "${project_root}/Cargo.toml"
    --package camera-toolbox-eeprom-helper
    --bin camera-toolbox-eeprom-helper
    --locked
    "${profile_args[@]}"
)

if (( calibration_enabled )); then
    python_command=${PYTHON:-python3}
    dependency_tool="${project_root}/scripts/opencv5_dependency.py"
    if ! command -v "$python_command" >/dev/null 2>&1; then
        printf 'error: Python interpreter not found: %s\n' "$python_command" >&2
        exit 1
    fi
    "$python_command" "$dependency_tool" run -- cargo "${cargo_args[@]}"
    cargo "${helper_args[@]}"
    if [[ "$(uname -s)" == "Linux" && "$(uname -m)" == "aarch64" ]]; then
        install -m 755 \
            "${target_dir}/${profile}/camera-toolbox-eeprom-helper" \
            "${target_dir}/${profile}/camera-toolbox-eeprom-helper-linux-aarch64"
    fi
    "$python_command" "$dependency_tool" bundle \
        --destination "${target_dir}/${profile}"
else
    cargo "${cargo_args[@]}"
    cargo "${helper_args[@]}"
fi
