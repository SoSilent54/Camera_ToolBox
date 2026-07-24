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
CV610, SSH-managed providers, pinned FFmpeg 8.1.2, and pinned OpenCV 5
calibration enabled together. Verified native dependencies are cached under
`.deps/ffmpeg` and `.deps/opencv5`; their runtime libraries are copied beside
the executable. Set CAMERA_TOOLBOX_CALIBRATION=0 to omit only OpenCV.

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
python_command=${PYTHON:-}
if [[ -z $python_command ]]; then
    if [[ ${OS:-} == "Windows_NT" ]]; then
        python_command=python
    else
        python_command=python3
    fi
fi
if ! command -v "$python_command" >/dev/null 2>&1; then
    printf 'error: Python interpreter not found: %s\n' "$python_command" >&2
    exit 1
fi
ffmpeg_dependency_tool="${project_root}/scripts/ffmpeg_dependency.py"
opencv_dependency_tool="${project_root}/scripts/opencv5_dependency.py"

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

"$python_command" "$ffmpeg_dependency_tool" prepare
if (( calibration_enabled )); then
    "$python_command" "$opencv_dependency_tool" prepare
fi

cargo "${cargo_args[@]}"
if [[ "$(uname -s)" == "Linux" && "$(uname -m)" == "aarch64" ]]; then
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+crt-static" \
        cargo "${helper_args[@]}" --target aarch64-unknown-linux-gnu
    install -m 755 \
        "${target_dir}/aarch64-unknown-linux-gnu/${profile}/camera-toolbox-eeprom-helper" \
        "${target_dir}/${profile}/camera-toolbox-eeprom-helper-linux-aarch64"
else
    cargo "${helper_args[@]}"
fi

"$python_command" "$ffmpeg_dependency_tool" bundle \
    --destination "${target_dir}/${profile}"
if (( calibration_enabled )); then
    "$python_command" "$opencv_dependency_tool" bundle \
        --destination "${target_dir}/${profile}"
fi
