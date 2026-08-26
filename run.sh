#!/usr/bin/env bash
# 开发运行入口：注入 FFmpeg/OpenCV 运行库路径后启动原生 Dear ImGui 前端。
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

ffmpeg_lib=$(grep -oP 'FFMPEG_DIR = \{ value = "\K[^"]+' "$ROOT/.cargo/ffmpeg.local.toml")/lib
opencv_lib=$(grep -oP 'CAMERA_TOOLBOX_OPENCV_RUNTIME_DIR = \{ value = "\K[^"]+' "$ROOT/.cargo/opencv5.local.toml")

export LD_LIBRARY_PATH="${ffmpeg_lib}:${opencv_lib}:${LD_LIBRARY_PATH:-}"
export PONGBOT_WRITE_HISTORY_DIR="${PONGBOT_WRITE_HISTORY_DIR:-$ROOT/write_history}"

exec cargo run --release -p pongbot-calib-tool -- "$@"
