#!/usr/bin/env python3
"""Prepare the pinned Camera Toolbox FFmpeg shared-library dependency."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import stat
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = "SoSilent54/Camera_ToolBox"
FFMPEG_VERSION = "8.1.2"
DEPENDENCY_REVISION = 1
RELEASE_TAG = f"ffmpeg-deps-v{FFMPEG_VERSION}-r{DEPENDENCY_REVISION}"
CARGO_ENV_CONFIG = PROJECT_ROOT / ".cargo/ffmpeg.local.toml"
CARGO_COMPILE_ENV_KEYS = ("FFMPEG_DIR", "PKG_CONFIG_PATH")
UNIX_RUNTIME_LIBRARY_PREFIXES = ("libavcodec", "libavformat", "libavutil", "libswscale", "libswresample")
WINDOWS_RUNTIME_LIBRARY_PREFIXES = ("avcodec", "avformat", "avutil", "swscale", "swresample")


class DependencyError(RuntimeError):
    pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("prepare", help="download, verify, and expose FFmpeg")
    prepare.add_argument("--github-env", type=Path)
    prepare.add_argument("--github-path", type=Path)
    run = commands.add_parser("run", help="run a command with the FFmpeg environment")
    run.add_argument("arguments", nargs=argparse.REMAINDER)
    bundle = commands.add_parser("bundle", help="copy FFmpeg runtime and licenses")
    bundle.add_argument("--destination", type=Path, required=True)
    verify = commands.add_parser("verify-archive", help="verify a locally produced FFmpeg archive")
    verify.add_argument("--platform", required=True)
    verify.add_argument("archive", type=Path)
    return parser.parse_args()


def normalized_architecture(value: str) -> str:
    value = value.lower()
    if value in {"x86_64", "amd64"}:
        return "x86_64"
    if value in {"aarch64", "arm64"}:
        return "aarch64"
    raise DependencyError(f"unsupported architecture: {value}")


def ubuntu_version() -> str:
    values: dict[str, str] = {}
    for line in Path("/etc/os-release").read_text(encoding="utf-8").splitlines():
        if "=" in line and not line.startswith("#"):
            key, value = line.split("=", 1)
            values[key] = value.strip().strip('"')
    if values.get("ID") != "ubuntu":
        raise DependencyError("FFmpeg dependency assets support Ubuntu Linux only")
    version = values.get("VERSION_ID", "")
    if version.startswith("20.04"):
        return "ubuntu20"
    if version.startswith("22.04"):
        return "ubuntu22"
    raise DependencyError(f"unsupported Ubuntu version: {version or 'unknown'}")


def detect_platform_id() -> str:
    override = os.environ.get("CAMERA_TOOLBOX_FFMPEG_PLATFORM")
    supported = {
        "windows-x86_64-msvc",
        "macos-aarch64-macos14",
        "linux-x86_64-ubuntu20",
        "linux-x86_64-ubuntu22",
        "linux-aarch64-ubuntu20",
        "linux-aarch64-ubuntu22",
    }
    if override:
        if override not in supported:
            raise DependencyError(f"unsupported FFmpeg dependency platform: {override}")
        return override
    system = platform.system()
    architecture = normalized_architecture(platform.machine())
    if system == "Windows" and architecture == "x86_64":
        return "windows-x86_64-msvc"
    if system == "Darwin" and architecture == "aarch64":
        return "macos-aarch64-macos14"
    if system == "Linux":
        return f"linux-{architecture}-{ubuntu_version()}"
    raise DependencyError(f"unsupported operating system: {system}")


def archive_name(platform_id: str) -> str:
    suffix = "zip" if platform_id.startswith("windows-") else "tar.gz"
    return f"ffmpeg-{FFMPEG_VERSION}-r{DEPENDENCY_REVISION}-{platform_id}.{suffix}"


def release_asset_url(name: str) -> str:
    return f"https://github.com/{REPOSITORY}/releases/download/{RELEASE_TAG}/{name}"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def download(url: str, destination: Path) -> None:
    temporary = destination.with_suffix(destination.suffix + ".tmp")
    try:
        with urllib.request.urlopen(url, timeout=120) as response, temporary.open("wb") as stream:
            shutil.copyfileobj(response, stream, length=1024 * 1024)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def expected_checksum(checksums: Path, asset: str) -> str:
    for line in checksums.read_text(encoding="utf-8").splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1].lstrip("*") == asset:
            value = fields[0].lower()
            if len(value) == 64 and all(char in "0123456789abcdef" for char in value):
                return value
    raise DependencyError(f"SHA256SUMS does not contain {asset}")


def is_safe_member(name: str) -> bool:
    path = Path(name)
    return not path.is_absolute() and ".." not in path.parts


def extract(archive: Path, destination: Path) -> None:
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            members = source.infolist()
            for member in members:
                mode = member.external_attr >> 16
                if not is_safe_member(member.filename) or stat.S_ISLNK(mode):
                    raise DependencyError("unsafe path or symlink in FFmpeg zip archive")
            for member in members:
                source.extract(member, destination)
        return

    with tarfile.open(archive, "r:gz") as source:
        members = source.getmembers()
        for member in members:
            if (
                not is_safe_member(member.name)
                or member.issym()
                or member.islnk()
                or member.isdev()
            ):
                raise DependencyError("unsafe path or link in FFmpeg tar archive")
        for member in members:
            source.extract(member, destination)


def runtime_library_prefixes(platform_id: str) -> tuple[str, ...]:
    if platform_id == "windows-x86_64-msvc":
        return WINDOWS_RUNTIME_LIBRARY_PREFIXES
    return UNIX_RUNTIME_LIBRARY_PREFIXES


def require_layout(root: Path, platform_id: str) -> tuple[Path, Path, Path, Path]:
    include_dir = root / "include"
    lib_dir = root / "lib"
    runtime_dir = root / "runtime"
    license_file = root / "licenses" / "FFMPEG-LICENSE"
    required_headers = [include_dir / "libavcodec" / "avcodec.h", include_dir / "libavformat" / "avformat.h"]
    if not all(path.is_file() for path in required_headers):
        raise DependencyError("FFmpeg archive is missing required headers")
    if not lib_dir.is_dir() or not runtime_dir.is_dir() or not license_file.is_file():
        raise DependencyError("FFmpeg archive has an invalid lib/runtime/license layout")
    required_prefixes = runtime_library_prefixes(platform_id)
    if not all(any(path.name.startswith(prefix) for path in runtime_dir.iterdir()) for prefix in required_prefixes):
        raise DependencyError("FFmpeg runtime does not contain all required libav libraries")
    if platform_id == "windows-x86_64-msvc":
        required_import_libraries = (
            "avcodec.lib",
            "avformat.lib",
            "avutil.lib",
            "swscale.lib",
            "swresample.lib",
        )
        if not all((lib_dir / name).is_file() for name in required_import_libraries):
            raise DependencyError("FFmpeg Windows archive is missing required MSVC import libraries")
    return include_dir, lib_dir, runtime_dir, license_file


def prepare() -> tuple[Path, Path, Path, Path]:
    platform_id = detect_platform_id()
    asset = archive_name(platform_id)
    cache = PROJECT_ROOT / ".deps" / "ffmpeg" / platform_id
    downloads = cache / "downloads"
    archive = downloads / asset
    checksums = downloads / "SHA256SUMS"
    downloads.mkdir(parents=True, exist_ok=True)
    if not checksums.is_file():
        download(release_asset_url("SHA256SUMS"), checksums)
    expected = expected_checksum(checksums, asset)
    if not archive.is_file() or sha256(archive) != expected:
        archive.unlink(missing_ok=True)
        download(release_asset_url(asset), archive)
        actual = sha256(archive)
        if actual != expected:
            archive.unlink(missing_ok=True)
            raise DependencyError(f"FFmpeg archive checksum mismatch: expected {expected}, got {actual}")
    root = cache / "ffmpeg"
    if not root.is_dir():
        staging = Path(tempfile.mkdtemp(prefix="ffmpeg-extract-", dir=cache))
        try:
            extract(archive, staging)
            extracted = staging / "ffmpeg"
            require_layout(extracted, platform_id)
            os.replace(extracted, root)
        finally:
            shutil.rmtree(staging, ignore_errors=True)
    return (root, *require_layout(root, platform_id))


def prepend(value: str, existing: str | None) -> str:
    return value if not existing else f"{value}{os.pathsep}{existing}"


def environment(root: Path, lib_dir: Path, runtime_dir: Path) -> dict[str, str]:
    updates = {
        "FFMPEG_DIR": str(root),
        "PKG_CONFIG_PATH": prepend(str(lib_dir / "pkgconfig"), os.environ.get("PKG_CONFIG_PATH")),
    }
    if platform.system() == "Windows":
        updates["PATH"] = prepend(str(runtime_dir), os.environ.get("PATH"))
    elif platform.system() == "Darwin":
        updates["DYLD_FALLBACK_LIBRARY_PATH"] = prepend(str(runtime_dir), os.environ.get("DYLD_FALLBACK_LIBRARY_PATH"))
    else:
        updates["LD_LIBRARY_PATH"] = prepend(str(runtime_dir), os.environ.get("LD_LIBRARY_PATH"))
    return updates

def write_cargo_environment(updates: dict[str, str]) -> None:
    CARGO_ENV_CONFIG.parent.mkdir(parents=True, exist_ok=True)
    lines = ["# Generated by scripts/ffmpeg_dependency.py; do not commit.", "[env]"]
    for key in CARGO_COMPILE_ENV_KEYS:
        value = updates.get(key)
        if value is not None:
            if "\n" in value or "\r" in value:
                raise DependencyError(f"environment value contains a newline: {key}")
            lines.append(f"{key} = {{ value = {json.dumps(value)}, force = true }}")
    payload = "\n".join(lines) + "\n"
    if CARGO_ENV_CONFIG.exists() and CARGO_ENV_CONFIG.read_text(encoding="utf-8") == payload:
        return
    temporary = CARGO_ENV_CONFIG.with_suffix(".tmp")
    try:
        temporary.write_text(payload, encoding="utf-8")
        os.replace(temporary, CARGO_ENV_CONFIG)
    finally:
        temporary.unlink(missing_ok=True)


def write_github_environment(updates: dict[str, str], github_env: Path | None, github_path: Path | None) -> None:
    if (github_env is None) != (github_path is None):
        raise DependencyError("--github-env and --github-path must be provided together")
    if github_env is None or github_path is None:
        return
    with github_env.open("a", encoding="utf-8") as stream:
        for key, value in updates.items():
            if key != "PATH":
                stream.write(f"{key}={value}\n")
    with github_path.open("a", encoding="utf-8") as stream:
        stream.write(f"{environment_runtime_path(updates)}\n")


def environment_runtime_path(updates: dict[str, str]) -> str:
    runtime_value = (
        updates.get("PATH")
        or updates.get("LD_LIBRARY_PATH")
        or updates.get("DYLD_FALLBACK_LIBRARY_PATH")
        or ""
    )
    return runtime_value.split(os.pathsep)[0]


def verify_archive(archive: Path, platform_id: str) -> None:
    if not archive.is_file():
        raise DependencyError(f"FFmpeg archive does not exist: {archive}")
    with tempfile.TemporaryDirectory(prefix="ffmpeg-archive-verify-") as temporary:
        extracted = Path(temporary)
        extract(archive, extracted)
        require_layout(extracted / "ffmpeg", platform_id)


def main() -> None:
    args = parse_args()
    if args.command == "verify-archive":
        verify_archive(args.archive, args.platform)
        return
    root, _include_dir, lib_dir, runtime_dir, license_file = prepare()
    updates = environment(root, lib_dir, runtime_dir)
    write_cargo_environment(updates)
    if args.command == "prepare":
        write_github_environment(updates, args.github_env, args.github_path)
        return
    if args.command == "run":
        if not args.arguments or args.arguments[0] != "--":
            raise DependencyError("run requires -- followed by a command")
        subprocess.run(args.arguments[1:], check=True, env=os.environ | updates)
        return
    destination = args.destination
    destination.mkdir(parents=True, exist_ok=True)
    for library in runtime_dir.iterdir():
        if library.is_file():
            shutil.copy2(library, destination / library.name)
    shutil.copy2(license_file, destination / "FFMPEG-LICENSE")


if __name__ == "__main__":
    try:
        main()
    except DependencyError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2) from error
