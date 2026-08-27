#!/usr/bin/env python3
"""Prepare and consume the pinned Camera Toolbox OpenCV 5 dependency."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping, Sequence

PROJECT_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_REPOSITORY = "SoSilent54/Camera_ToolBox"
OPENCV_VERSION = "5.0.0"
OPENCV_COMMIT = "40738fb16ceddb5fb3fea747585f7ce6abb0605b"
DEPENDENCY_REVISION = 2
RELEASE_TAG = f"opencv-deps-v{OPENCV_VERSION}-r{DEPENDENCY_REVISION}"
CARGO_ENV_CONFIG = PROJECT_ROOT / ".cargo/opencv5.local.toml"
CARGO_COMPILE_ENV_KEYS = (
    "CAMERA_TOOLBOX_OPENCV_ROOT",
    "CAMERA_TOOLBOX_OPENCV_RUNTIME_DIR",
    "LIBCLANG_PATH",
    "PYTHON",
    "OPENCV_INCLUDE_PATHS",
    "OPENCV_LINK_LIBS",
    "OPENCV_LINK_PATHS",
    "OPENCV_MSVC_CRT",
)

MODULES = [
    "core",
    "imgproc",
    "imgcodecs",
    "features",
    "flann",
    "geometry",
    "objdetect",
    "stereo",
    "calib",
    "world",
]


@dataclass(frozen=True)
class DependencySpec:
    platform_id: str
    sha256: str
    archive_format: str
    link_name: str

    @property
    def archive_name(self) -> str:
        return (
            f"opencv-{OPENCV_VERSION}-r{DEPENDENCY_REVISION}-"
            f"{self.platform_id}.{self.archive_format}"
        )


@dataclass(frozen=True)
class DependencyLayout:
    spec: DependencySpec
    root: Path
    include_dir: Path
    lib_dir: Path
    runtime_dir: Path
    runtime_files: tuple[Path, ...]
    license_file: Path


SPECS = {
    "windows-x86_64-msvc": DependencySpec(
        platform_id="windows-x86_64-msvc",
        sha256="e3f8ae7c69fd5ae3893d7e42c1d555bc2e3dc1a9c517017164d3448518f0bc53",
        archive_format="zip",
        link_name="opencv_world500",
    ),
    "macos-aarch64-macos14": DependencySpec(
        platform_id="macos-aarch64-macos14",
        sha256="fcbb9bfa2b86636a2eee28aa0bf70ad1e48edaf3f653a9a67d336e7794da98f4",
        archive_format="tar.gz",
        link_name="opencv_world",
    ),
    "linux-x86_64-ubuntu20": DependencySpec(
        platform_id="linux-x86_64-ubuntu20",
        sha256="e4495954bc1a6001466fed7090a039a3e799ab5bc8e7f66b92a2ef31f6f126c9",
        archive_format="tar.gz",
        link_name="opencv_world",
    ),
    "linux-x86_64-ubuntu22": DependencySpec(
        platform_id="linux-x86_64-ubuntu22",
        sha256="b4806b833893a96fe6fb2328a7c745f96475d446aed78312f1c8b2001040f666",
        archive_format="tar.gz",
        link_name="opencv_world",
    ),
    "linux-aarch64-ubuntu20": DependencySpec(
        platform_id="linux-aarch64-ubuntu20",
        sha256="cd8a0de1063c2bf2231f31042bd7ee600d0b21e4b5fb80dacd6eaab644ad64ea",
        archive_format="tar.gz",
        link_name="opencv_world",
    ),
    "linux-aarch64-ubuntu22": DependencySpec(
        platform_id="linux-aarch64-ubuntu22",
        sha256="59d43e5cc1d6cb1645b1554fea5b27d21c92fc532564710d73d7b71aae8429d0",
        archive_format="tar.gz",
        link_name="opencv_world",
    ),
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    prepare = subparsers.add_parser("prepare", help="prepare and validate the dependency")
    prepare.add_argument("--github-env", type=Path)
    prepare.add_argument("--github-path", type=Path)
    prepare.add_argument("--json", action="store_true", dest="print_json")

    run = subparsers.add_parser("run", help="run a command with the dependency environment")
    run.add_argument("arguments", nargs=argparse.REMAINDER)

    bundle = subparsers.add_parser(
        "bundle", help="copy runtime libraries and the OpenCV license into a bundle"
    )
    bundle.add_argument("--destination", type=Path, required=True)

    return parser.parse_args()


def normalized_architecture(value: str) -> str:
    normalized = value.lower()
    if normalized in {"x86_64", "amd64"}:
        return "x86_64"
    if normalized in {"aarch64", "arm64"}:
        return "aarch64"
    raise RuntimeError(f"unsupported architecture: {value}")


def read_os_release(path: Path = Path("/etc/os-release")) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key] = value.strip().strip('"')
    return values


def detect_platform_id() -> str:
    override = os.environ.get("CAMERA_TOOLBOX_OPENCV_PLATFORM")
    if override:
        if override not in SPECS:
            raise RuntimeError(f"unsupported OpenCV dependency platform: {override}")
        return override

    system = platform.system()
    architecture = normalized_architecture(platform.machine())
    if system == "Linux":
        release = read_os_release()
        if release.get("ID") != "ubuntu":
            raise RuntimeError("OpenCV dependency assets support Ubuntu Linux only")
        version = release.get("VERSION_ID", "")
        if version.startswith("20.04"):
            ubuntu = "ubuntu20"
        elif version.startswith("22.04"):
            ubuntu = "ubuntu22"
        else:
            raise RuntimeError(f"unsupported Ubuntu version: {version or 'unknown'}")
        return f"linux-{architecture}-{ubuntu}"
    if system == "Darwin":
        if architecture != "aarch64":
            raise RuntimeError("OpenCV dependency assets support Apple arm64 only")
        major_text = platform.mac_ver()[0].split(".", 1)[0]
        if not major_text or int(major_text) < 14:
            raise RuntimeError("OpenCV dependency requires macOS 14 or newer")
        return "macos-aarch64-macos14"
    if system == "Windows":
        if architecture != "x86_64":
            raise RuntimeError("OpenCV dependency assets support Windows x86_64 only")
        return "windows-x86_64-msvc"
    raise RuntimeError(f"unsupported operating system: {system}")


def cache_root() -> Path:
    configured = os.environ.get("CAMERA_TOOLBOX_OPENCV_CACHE")
    return Path(configured).expanduser().resolve() if configured else PROJECT_ROOT / ".deps/opencv5"


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def dependency_repository() -> str:
    """Return the repository that owns dependency GitHub Releases."""
    return (
        os.environ.get("CAMERA_TOOLBOX_DEPENDENCY_REPOSITORY")
        or os.environ.get("GITHUB_REPOSITORY")
        or DEFAULT_REPOSITORY
    )


def github_token() -> str | None:
    """Return an optional token for private repository release assets."""
    return (
        os.environ.get("CAMERA_TOOLBOX_GITHUB_TOKEN")
        or os.environ.get("GH_TOKEN")
        or os.environ.get("GITHUB_TOKEN")
    )


def github_request(url: str, *, accept: str) -> urllib.request.Request:
    headers = {
        "Accept": accept,
        "User-Agent": "camera-toolbox-opencv5-consumer",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    token = github_token()
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return urllib.request.Request(url, headers=headers)


class CrossHostAuthorizationRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Strip GitHub tokens before following redirects to asset storage hosts."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
        if redirected is None:
            return None
        old_host = urllib.parse.urlparse(req.full_url).netloc.lower()
        new_host = urllib.parse.urlparse(newurl).netloc.lower()
        if old_host != new_host:
            redirected.remove_header("Authorization")
        return redirected


def open_github_request(request: urllib.request.Request, *, timeout: int):
    opener = urllib.request.build_opener(CrossHostAuthorizationRedirectHandler)
    return opener.open(request, timeout=timeout)


def release_asset_browser_url(spec: DependencySpec) -> str:
    return (
        f"https://github.com/{dependency_repository()}/releases/download/"
        f"{RELEASE_TAG}/{spec.archive_name}"
    )


def release_asset_api_url(spec: DependencySpec) -> str:
    release_url = (
        f"https://api.github.com/repos/{dependency_repository()}/releases/tags/"
        f"{RELEASE_TAG}"
    )
    with open_github_request(
        github_request(release_url, accept="application/vnd.github+json"), timeout=120
    ) as response:
        release = json.load(response)
    for asset in release.get("assets", []):
        if asset.get("name") == spec.archive_name:
            return str(asset["url"])
    raise RuntimeError(
        f"OpenCV dependency asset not found in GitHub Release {dependency_repository()} "
        f"{RELEASE_TAG}: {spec.archive_name}"
    )


def release_asset_request(spec: DependencySpec) -> urllib.request.Request:
    if github_token():
        return github_request(release_asset_api_url(spec), accept="application/octet-stream")
    return github_request(release_asset_browser_url(spec), accept="application/octet-stream")


def download_error_message(spec: DependencySpec, error: urllib.error.HTTPError) -> str:
    auth_state = "authenticated" if github_token() else "unauthenticated"
    return (
        f"failed to download OpenCV dependency asset {spec.archive_name} from "
        f"GitHub Release {dependency_repository()} {RELEASE_TAG}: "
        f"HTTP {error.code} {error.reason} ({auth_state}). "
        "If the repository is private, pass GH_TOKEN/GITHUB_TOKEN with contents:read. "
        "If only the Actions artifact is marked Expired, rerun the OpenCV 5 Dependencies "
        "workflow and ensure the GitHub Release assets were published."
    )


def asset_url(spec: DependencySpec) -> str:
    return release_asset_browser_url(spec)


def download_archive(spec: DependencySpec, root: Path) -> Path:
    downloads = root / "downloads"
    downloads.mkdir(parents=True, exist_ok=True)
    archive_path = downloads / spec.archive_name
    if archive_path.is_file() and file_sha256(archive_path) == spec.sha256:
        return archive_path
    if archive_path.exists():
        archive_path.unlink()

    temporary = downloads / f".{spec.archive_name}.{os.getpid()}.tmp"
    print(f"Downloading {spec.archive_name}", file=sys.stderr)
    try:
        try:
            with open_github_request(
                release_asset_request(spec), timeout=120
            ) as response, temporary.open("wb") as out:
                shutil.copyfileobj(response, out, length=1024 * 1024)
        except urllib.error.HTTPError as error:
            raise RuntimeError(download_error_message(spec, error)) from error
        actual = file_sha256(temporary)
        if actual != spec.sha256:
            raise RuntimeError(
                f"OpenCV dependency checksum mismatch: expected {spec.sha256}, got {actual}"
            )
        os.replace(temporary, archive_path)
    finally:
        if temporary.exists():
            temporary.unlink()
    return archive_path


def is_within(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
    except ValueError:
        return False
    return True


def validate_member_path(destination: Path, name: str) -> Path:
    member = Path(name)
    if member.is_absolute() or ".." in member.parts:
        raise RuntimeError(f"archive contains an unsafe path: {name}")
    target = (destination / member).resolve()
    if not is_within(target, destination):
        raise RuntimeError(f"archive path escapes destination: {name}")
    return target


def extract_tar_gz(archive_path: Path, destination: Path) -> None:
    with tarfile.open(archive_path, mode="r:gz") as archive:
        members = archive.getmembers()
        for member in members:
            target = validate_member_path(destination, member.name)
            if not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
                raise RuntimeError(f"archive contains an unsupported entry: {member.name}")
            if member.issym() or member.islnk():
                link = Path(member.linkname)
                if link.is_absolute():
                    raise RuntimeError(f"archive contains an absolute link: {member.name}")
                link_base = target.parent if member.issym() else destination
                link_target = (link_base / link).resolve()
                if not is_within(link_target, destination):
                    raise RuntimeError(f"archive link escapes destination: {member.name}")
        archive.extractall(destination, members=members)


def extract_zip(archive_path: Path, destination: Path) -> None:
    with zipfile.ZipFile(archive_path, mode="r") as archive:
        for member in archive.infolist():
            validate_member_path(destination, member.filename)
            mode = member.external_attr >> 16
            if stat.S_ISLNK(mode):
                raise RuntimeError(f"zip archive contains a symlink: {member.filename}")
        archive.extractall(destination)


def one_path(paths: Iterable[Path], description: str) -> Path:
    candidates = sorted({path.absolute() for path in paths})
    if len(candidates) != 1:
        rendered = ", ".join(str(path) for path in candidates) or "none"
        raise RuntimeError(f"expected one {description}, found: {rendered}")
    return candidates[0]


def validate_manifest(root: Path, spec: DependencySpec) -> None:
    manifest_path = root / "camera-toolbox-opencv-dependency.json"
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"invalid OpenCV dependency manifest: {manifest_path}") from error
    expected: Mapping[str, object] = {
        "schema_version": 1,
        "name": "camera-toolbox-opencv-dependency",
        "opencv_version": OPENCV_VERSION,
        "opencv_commit": OPENCV_COMMIT,
        "dependency_revision": DEPENDENCY_REVISION,
        "platform": spec.platform_id,
        "linkage": "shared",
        "codec_policy": "bundled-png-zlib-only",
        "modules": MODULES,
    }
    for key, value in expected.items():
        if manifest.get(key) != value:
            raise RuntimeError(
                f"OpenCV dependency manifest mismatch for {key}: "
                f"expected {value!r}, got {manifest.get(key)!r}"
            )


def discover_layout(root: Path, spec: DependencySpec) -> DependencyLayout:
    validate_manifest(root, spec)
    version_header = one_path(
        root.rglob("opencv2/core/version.hpp"), "OpenCV version header"
    )
    include_dir = version_header.parents[2]
    for header in ("opencv2/calib.hpp", "opencv2/geometry.hpp", "opencv2/objdetect.hpp"):
        if not (include_dir / header).is_file():
            raise RuntimeError(f"OpenCV dependency header is missing: {header}")

    if spec.platform_id.startswith("windows-"):
        link_library = one_path(
            (
                path
                for path in root.rglob("*.lib")
                if path.name.lower() == "opencv_world500.lib"
            ),
            "OpenCV world import library",
        )
        runtime_files = (
            one_path(
                (
                    path
                    for path in root.rglob("*.dll")
                    if path.name.lower() == "opencv_world500.dll"
                ),
                "OpenCV world runtime library",
            ),
        )
    elif spec.platform_id.startswith("macos-"):
        link_library = one_path(root.rglob("libopencv_world.dylib"), "OpenCV world link library")
        runtime_files = tuple(sorted(root.rglob("libopencv_world*.dylib")))
    else:
        link_library = one_path(root.rglob("libopencv_world.so"), "OpenCV world link library")
        runtime_files = tuple(sorted(root.rglob("libopencv_world.so*")))

    if not runtime_files or any(not path.is_file() for path in runtime_files):
        raise RuntimeError("OpenCV world runtime library set is incomplete")
    runtime_dirs = {path.parent.absolute() for path in runtime_files}
    if len(runtime_dirs) != 1:
        raise RuntimeError(f"OpenCV runtime libraries span multiple directories: {runtime_dirs}")
    license_file = one_path(
        root.rglob("share/licenses/opencv5/LICENSE"), "OpenCV license"
    )
    return DependencyLayout(
        spec=spec,
        root=root.absolute(),
        include_dir=include_dir.absolute(),
        lib_dir=link_library.parent.absolute(),
        runtime_dir=next(iter(runtime_dirs)),
        runtime_files=runtime_files,
        license_file=license_file,
    )


def prepare_dependency() -> DependencyLayout:
    platform_id = detect_platform_id()
    spec = SPECS[platform_id]
    root = cache_root()
    prepared_parent = root / platform_id
    prepared_root = prepared_parent / "opencv"
    if prepared_root.is_dir():
        try:
            return discover_layout(prepared_root, spec)
        except RuntimeError:
            shutil.rmtree(prepared_parent)

    archive_path = download_archive(spec, root)
    staging = Path(tempfile.mkdtemp(prefix=f".{platform_id}-", dir=root))
    try:
        extracted = staging / "extracted"
        extracted.mkdir()
        if spec.archive_format == "tar.gz":
            extract_tar_gz(archive_path, extracted)
        else:
            extract_zip(archive_path, extracted)
        staged_root = extracted / "opencv"
        layout = discover_layout(staged_root, spec)
        if prepared_parent.exists():
            shutil.rmtree(prepared_parent)
        prepared_parent.mkdir(parents=True)
        os.replace(staged_root, prepared_root)
        return discover_layout(prepared_root, spec)
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def find_libclang_dir() -> Path:
    configured = os.environ.get("LIBCLANG_PATH")
    if configured:
        path = Path(configured)
        if path.is_dir():
            return path.absolute()
        raise RuntimeError(f"LIBCLANG_PATH is not a directory: {path}")

    candidates: list[Path] = []
    system = platform.system()
    if system == "Linux":
        candidates.extend(Path("/usr/lib").glob("llvm-*/lib"))
    elif system == "Darwin":
        candidates.extend(
            [
                Path("/opt/homebrew/opt/llvm/lib"),
                Path("/usr/local/opt/llvm/lib"),
                # GitHub Actions macOS runner（无 Homebrew LLVM）自带 CLT/Xcode 的 libclang。
                Path("/Library/Developer/CommandLineTools/usr/lib"),
                Path(
                    "/Applications/Xcode.app/Contents/Developer/Toolchains/"
                    "XcodeDefault.xctoolchain/usr/lib"
                ),
            ]
        )
    elif system == "Windows":
        candidates.append(Path("C:/Program Files/LLVM/bin"))
    for candidate in reversed(sorted(candidates)):
        if candidate.is_dir() and any(candidate.glob("libclang.*")):
            return candidate.absolute()
    raise RuntimeError(
        "libclang was not found; install clang/libclang and set LIBCLANG_PATH"
    )


def dependency_environment(layout: DependencyLayout) -> dict[str, str]:
    updates = {
        "CAMERA_TOOLBOX_OPENCV_ROOT": str(layout.root),
        "CAMERA_TOOLBOX_OPENCV_RUNTIME_DIR": str(layout.runtime_dir),
        "LIBCLANG_PATH": str(find_libclang_dir()),
        "PYTHON": sys.executable,
        "OPENCV_DISABLE_PROBES": "pkg_config,cmake,vcpkg_cmake,vcpkg",
        "OPENCV_INCLUDE_PATHS": str(layout.include_dir),
        "OPENCV_LINK_LIBS": layout.spec.link_name,
        "OPENCV_LINK_PATHS": str(layout.lib_dir),
    }
    system = platform.system()
    if system == "Windows":
        updates["OPENCV_MSVC_CRT"] = "dynamic"
        updates["PATH"] = prepend_path(str(layout.runtime_dir), os.environ.get("PATH"))
    elif system == "Darwin":
        updates["DYLD_FALLBACK_LIBRARY_PATH"] = prepend_path(
            str(layout.runtime_dir), os.environ.get("DYLD_FALLBACK_LIBRARY_PATH")
        )
    else:
        updates["LD_LIBRARY_PATH"] = prepend_path(
            str(layout.runtime_dir), os.environ.get("LD_LIBRARY_PATH")
        )
    return updates


def prepend_path(value: str, existing: str | None) -> str:
    return value if not existing else f"{value}{os.pathsep}{existing}"


def write_cargo_environment(updates: Mapping[str, str]) -> None:
    """Persist compile-time dependency paths for subsequent direct Cargo commands."""
    CARGO_ENV_CONFIG.parent.mkdir(parents=True, exist_ok=True)
    lines = [
        "# Generated by scripts/opencv5_dependency.py; do not commit.",
        "[env]",
    ]
    for key in CARGO_COMPILE_ENV_KEYS:
        value = updates.get(key)
        if value is None:
            continue
        if "\n" in value or "\r" in value:
            raise RuntimeError(f"environment value contains a newline: {key}")
        lines.append(
            f"{key} = {{ value = {json.dumps(value)}, force = true }}"
        )
    payload = "\n".join(lines) + "\n"
    if CARGO_ENV_CONFIG.exists() and CARGO_ENV_CONFIG.read_text(encoding="utf-8") == payload:
        return

    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            newline="\n",
            dir=CARGO_ENV_CONFIG.parent,
            prefix=".opencv5.local.",
            suffix=".tmp",
            delete=False,
        ) as stream:
            stream.write(payload)
            temporary_path = Path(stream.name)
        os.replace(temporary_path, CARGO_ENV_CONFIG)
        temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def write_github_files(
    updates: Mapping[str, str], github_env: Path | None, github_path: Path | None
) -> None:
    if (github_env is None) != (github_path is None):
        raise RuntimeError("--github-env and --github-path must be provided together")
    if github_env is None or github_path is None:
        return
    with github_env.open("a", encoding="utf-8", newline="\n") as stream:
        for key, value in updates.items():
            if key == "PATH":
                continue
            if "\n" in value or "\r" in value:
                raise RuntimeError(f"environment value contains a newline: {key}")
            stream.write(f"{key}={value}\n")
    with github_path.open("a", encoding="utf-8", newline="\n") as stream:
        stream.write(f"{updates['CAMERA_TOOLBOX_OPENCV_RUNTIME_DIR']}\n")


def layout_json(layout: DependencyLayout) -> dict[str, object]:
    return {
        "platform": layout.spec.platform_id,
        "archive": layout.spec.archive_name,
        "sha256": layout.spec.sha256,
        "root": str(layout.root),
        "include_dir": str(layout.include_dir),
        "lib_dir": str(layout.lib_dir),
        "runtime_dir": str(layout.runtime_dir),
        "runtime_files": [str(path) for path in layout.runtime_files],
        "link_lib": layout.spec.link_name,
    }


def copy_runtime_file(source: Path, destination: Path) -> None:
    if destination.exists() or destination.is_symlink():
        destination.unlink()
    if source.is_symlink():
        destination.symlink_to(os.readlink(source))
    else:
        shutil.copy2(source, destination)


def bundle_dependency(layout: DependencyLayout, destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    for source in layout.runtime_files:
        target = destination / source.name
        copy_runtime_file(source, target)
        print(f"Bundled {target}")
    license_target = destination / "licenses/opencv5/LICENSE"
    license_target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(layout.license_file, license_target)
    print(f"Bundled {license_target}")


def normalized_command(arguments: Sequence[str]) -> list[str]:
    command = list(arguments)
    if command and command[0] == "--":
        command.pop(0)
    if not command:
        raise RuntimeError("run requires a command after --")
    return command


def main() -> int:
    args = parse_args()
    layout = prepare_dependency()
    if args.command == "bundle":
        bundle_dependency(layout, args.destination.resolve())
        return 0

    updates = dependency_environment(layout)
    write_cargo_environment(updates)
    if args.command == "prepare":
        write_github_files(updates, args.github_env, args.github_path)
        if args.print_json:
            print(json.dumps(layout_json(layout), indent=2, sort_keys=True))
        else:
            print(
                f"Prepared OpenCV {OPENCV_VERSION} dependency: "
                f"{layout.spec.platform_id} at {layout.root}"
            )
        return 0

    command = normalized_command(args.arguments)
    environment = os.environ.copy()
    environment.update(updates)
    completed = subprocess.run(command, check=False, env=environment)
    return completed.returncode


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
