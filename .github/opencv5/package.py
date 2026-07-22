#!/usr/bin/env python3
"""Package and relocate-test one OpenCV 5 dependency install tree."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import stat
import sys
import tarfile
import zipfile
from pathlib import Path
from typing import Iterable, Mapping, Sequence

METADATA_SUFFIXES = {".cmake", ".la", ".pc"}
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


def parse_positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("revision must be a positive integer")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scratch-root", type=Path, required=True)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--platform-id", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--revision", type=parse_positive_int, required=True)
    parser.add_argument("--archive-format", choices=("tar.gz", "zip"), required=True)
    return parser.parse_args()


def is_within(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
    except ValueError:
        return False
    return True


def normalized_tokens(paths: Iterable[Path]) -> set[str]:
    tokens: set[str] = set()
    for path in paths:
        text = str(path.resolve())
        tokens.add(text)
        tokens.add(text.replace("\\", "/"))
        tokens.add(text.replace("/", "\\"))
    return {token for token in tokens if token}


def run_command(
    command: Sequence[object], *, env: Mapping[str, str] | None = None
) -> str:
    rendered = [str(part) for part in command]
    completed = subprocess.run(
        rendered,
        check=False,
        env=None if env is None else dict(env),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    print(f"$ {' '.join(rendered)}")
    print(completed.stdout, end="" if completed.stdout.endswith("\n") else "\n")
    if completed.returncode != 0:
        raise RuntimeError(
            f"command failed with exit code {completed.returncode}: {' '.join(rendered)}"
        )
    return completed.stdout


def normalize_pkg_config(package_root: Path) -> None:
    for path in sorted(package_root.rglob("*.pc")):
        text = path.read_text(encoding="utf-8")
        relative_prefix = os.path.relpath(package_root, path.parent).replace(os.sep, "/")
        if relative_prefix == ".":
            prefix = "${pcfiledir}"
        else:
            prefix = f"${{pcfiledir}}/{relative_prefix}"
        lines = text.splitlines()
        replaced = False
        for index, line in enumerate(lines):
            if line.startswith("prefix="):
                lines[index] = f"prefix={prefix}"
                replaced = True
                break
        if not replaced:
            raise RuntimeError(f"pkg-config metadata has no prefix entry: {path}")
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def assert_metadata_relocatable(package_root: Path, forbidden: set[str]) -> None:
    violations: list[str] = []
    for path in sorted(package_root.rglob("*")):
        if not path.is_file() or path.suffix.lower() not in METADATA_SUFFIXES:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError as error:
            raise RuntimeError(f"metadata is not UTF-8: {path}") from error
        matched = sorted(token for token in forbidden if token in text)
        if matched:
            violations.append(f"{path}: {matched[0]}")
    if violations:
        details = "\n".join(violations)
        raise RuntimeError(f"non-relocatable metadata paths detected:\n{details}")


def write_manifest(
    package_root: Path,
    *,
    version: str,
    commit: str,
    revision: int,
    platform_id: str,
) -> None:
    manifest = {
        "schema_version": 1,
        "name": "camera-toolbox-opencv-dependency",
        "opencv_version": version,
        "opencv_commit": commit,
        "dependency_revision": revision,
        "platform": platform_id,
        "linkage": "shared",
        "modules": MODULES,
        "codec_policy": "bundled-png-zlib-only",
        "workflow_repository": os.environ.get("GITHUB_REPOSITORY", "local"),
        "workflow_commit": os.environ.get("GITHUB_SHA", "local"),
        "workflow_run_id": os.environ.get("GITHUB_RUN_ID", "local"),
    }
    path = package_root / "camera-toolbox-opencv-dependency.json"
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def stage_install_tree(
    install_root: Path,
    source_root: Path,
    stage_root: Path,
    *,
    version: str,
    commit: str,
    revision: int,
    platform_id: str,
) -> Path:
    if not install_root.is_dir():
        raise RuntimeError(f"OpenCV install root does not exist: {install_root}")
    package_root = stage_root / "opencv"
    shutil.copytree(install_root, package_root, symlinks=True)

    source_license = source_root / "LICENSE"
    if not source_license.is_file():
        raise RuntimeError(f"OpenCV license is missing: {source_license}")
    license_dir = package_root / "share" / "licenses" / "opencv5"
    license_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source_license, license_dir / "LICENSE")

    normalize_pkg_config(package_root)
    write_manifest(
        package_root,
        version=version,
        commit=commit,
        revision=revision,
        platform_id=platform_id,
    )
    return package_root


def create_tar_gz(package_root: Path, archive_path: Path) -> None:
    with tarfile.open(archive_path, mode="w:gz", format=tarfile.PAX_FORMAT) as archive:
        archive.add(package_root, arcname="opencv", recursive=True)


def create_zip(package_root: Path, archive_path: Path) -> None:
    with zipfile.ZipFile(
        archive_path, mode="w", compression=zipfile.ZIP_DEFLATED, compresslevel=9
    ) as archive:
        for path in sorted(package_root.rglob("*")):
            if path.is_symlink():
                raise RuntimeError(f"Windows dependency package contains a symlink: {path}")
            if path.is_file():
                archive.write(path, Path("opencv") / path.relative_to(package_root))


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


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
            if not (
                member.isfile()
                or member.isdir()
                or member.issym()
                or member.islnk()
            ):
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


def safe_remove(path: Path, scratch_root: Path) -> None:
    resolved = path.resolve()
    if resolved == scratch_root or not is_within(resolved, scratch_root):
        raise RuntimeError(f"refusing to remove path outside scratch root: {resolved}")
    if resolved.exists():
        shutil.rmtree(resolved)


def one_path(paths: Iterable[Path], description: str) -> Path:
    candidates = sorted({path.resolve() for path in paths})
    if len(candidates) != 1:
        rendered = ", ".join(str(path) for path in candidates) or "none"
        raise RuntimeError(f"expected one {description}, found: {rendered}")
    return candidates[0]


def discover_layout(package_root: Path, version: str) -> dict[str, Path | str]:
    version_header = one_path(
        package_root.rglob("opencv2/core/version.hpp"), "OpenCV version header"
    )
    include_dir = version_header.parents[2]
    system = platform.system()
    cmake_configs = list(package_root.rglob("OpenCVConfig.cmake"))
    if system == "Windows":
        # Windows 根配置会按当前架构和 MSVC 版本转发到嵌套配置。
        cmake_config = package_root / "OpenCVConfig.cmake"
        if not cmake_config.is_file():
            raise RuntimeError(f"Windows OpenCV root config is missing: {cmake_config}")
        one_path(
            (path for path in cmake_configs if path != cmake_config),
            "nested Windows OpenCVConfig.cmake",
        )
    else:
        cmake_config = one_path(cmake_configs, "OpenCVConfig.cmake")

    version_digits = version.replace(".", "")
    if system == "Windows":
        link_pattern = re.compile(rf"opencv_world{re.escape(version_digits)}\.lib$", re.IGNORECASE)
        runtime_pattern = re.compile(
            rf"opencv_world{re.escape(version_digits)}\.dll$", re.IGNORECASE
        )
        link_library = one_path(
            (path for path in package_root.rglob("*.lib") if link_pattern.fullmatch(path.name)),
            "OpenCV world import library",
        )
        runtime_library = one_path(
            (path for path in package_root.rglob("*.dll") if runtime_pattern.fullmatch(path.name)),
            "OpenCV world runtime library",
        )
        link_name = link_library.stem
    elif system == "Darwin":
        link_library = one_path(
            package_root.rglob("libopencv_world.dylib"), "OpenCV world link library"
        )
        runtime_library = link_library
        link_name = "opencv_world"
    elif system == "Linux":
        link_library = one_path(
            package_root.rglob("libopencv_world.so"), "OpenCV world link library"
        )
        runtime_library = link_library
        link_name = "opencv_world"
    else:
        raise RuntimeError(f"unsupported build host: {system}")

    if system != "Windows":
        one_path(package_root.rglob("opencv5.pc"), "opencv5.pc")

    return {
        "include_dir": include_dir,
        "cmake_dir": cmake_config.parent,
        "lib_dir": link_library.parent,
        "runtime_dir": runtime_library.parent,
        "link_lib": link_name,
        "world_library": link_library,
        "runtime_library": runtime_library,
    }


def reject_forbidden_output(output: str, forbidden: set[str], description: str) -> None:
    matched = sorted(token for token in forbidden if token in output)
    if matched:
        raise RuntimeError(f"{description} contains original path: {matched[0]}")


def verify_native_runtime(layout: Mapping[str, Path | str], forbidden: set[str]) -> None:
    system = platform.system()
    world_library = Path(layout["world_library"])
    runtime_library = Path(layout["runtime_library"])
    runtime_dir = str(layout["runtime_dir"])

    if system == "Linux":
        dynamic = run_command(("readelf", "-d", world_library))
        reject_forbidden_output(dynamic, forbidden, "readelf output")
        env = os.environ.copy()
        env["LD_LIBRARY_PATH"] = runtime_dir
        dependencies = run_command(("ldd", runtime_library), env=env)
        reject_forbidden_output(dependencies, forbidden, "ldd output")
        if "not found" in dependencies:
            raise RuntimeError("ldd reports an unresolved shared library")
    elif system == "Darwin":
        dylib_id = run_command(("otool", "-D", runtime_library))
        dependencies = run_command(("otool", "-L", runtime_library))
        reject_forbidden_output(dylib_id, forbidden, "dylib id")
        reject_forbidden_output(dependencies, forbidden, "dylib dependencies")
        id_lines = [line.strip() for line in dylib_id.splitlines()[1:] if line.strip()]
        if len(id_lines) != 1 or not id_lines[0].startswith(("@rpath/", "@loader_path/")):
            raise RuntimeError(f"dylib id is not relocatable: {id_lines}")
    elif system == "Windows":
        dependencies = run_command(("dumpbin", "/DEPENDENTS", runtime_library))
        reject_forbidden_output(dependencies, forbidden, "dumpbin output")
    else:
        raise RuntimeError(f"unsupported build host: {system}")


def write_github_outputs(values: Mapping[str, object]) -> None:
    output_path = os.environ.get("GITHUB_OUTPUT")
    if not output_path:
        return
    with Path(output_path).open("a", encoding="utf-8") as stream:
        for key, value in values.items():
            stream.write(f"{key}={value}\n")


def main() -> int:
    args = parse_args()
    if not re.fullmatch(r"[a-z0-9][a-z0-9._-]*", args.platform_id):
        raise RuntimeError(f"invalid platform id: {args.platform_id}")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
        raise RuntimeError(f"invalid OpenCV version: {args.version}")
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit):
        raise RuntimeError(f"invalid OpenCV commit: {args.commit}")

    scratch_root = args.scratch_root.resolve()
    source_root = args.source_root.resolve()
    output_dir = args.output_dir.resolve()
    if scratch_root == Path(scratch_root.anchor):
        raise RuntimeError("scratch root must not be a filesystem root")
    output_dir.mkdir(parents=True, exist_ok=True)
    scratch_root.mkdir(parents=True, exist_ok=True)

    build_root = scratch_root / "build"
    install_root = scratch_root / "install"
    stage_root = scratch_root / "stage"
    relocated_parent = scratch_root / "relocated"
    if stage_root.exists() or relocated_parent.exists():
        raise RuntimeError("staging or relocation directory already exists")

    package_root = stage_install_tree(
        install_root,
        source_root,
        stage_root,
        version=args.version,
        commit=args.commit,
        revision=args.revision,
        platform_id=args.platform_id,
    )
    forbidden = normalized_tokens((source_root, build_root, install_root, stage_root))
    assert_metadata_relocatable(package_root, forbidden)

    suffix = args.archive_format
    asset_name = (
        f"opencv-{args.version}-r{args.revision}-{args.platform_id}.{suffix}"
    )
    archive_path = output_dir / asset_name
    if archive_path.exists():
        raise RuntimeError(f"refusing to overwrite archive: {archive_path}")
    if args.archive_format == "tar.gz":
        create_tar_gz(package_root, archive_path)
    else:
        create_zip(package_root, archive_path)

    digest = sha256(archive_path)
    checksum_path = output_dir / f"{asset_name}.sha256"
    checksum_path.write_bytes(f"{digest}  {asset_name}\n".encode("ascii"))

    for path in (build_root, install_root, stage_root):
        safe_remove(path, scratch_root)
    relocated_parent.mkdir(parents=True)
    if args.archive_format == "tar.gz":
        extract_tar_gz(archive_path, relocated_parent)
    else:
        extract_zip(archive_path, relocated_parent)

    relocated_root = (relocated_parent / "opencv").resolve()
    if not relocated_root.is_dir():
        raise RuntimeError("archive does not contain the required opencv/ root")
    assert_metadata_relocatable(relocated_root, forbidden)
    layout = discover_layout(relocated_root, args.version)
    verify_native_runtime(layout, forbidden)

    outputs: dict[str, object] = {
        "asset": archive_path.resolve(),
        "asset_name": asset_name,
        "checksum": checksum_path.resolve(),
        "checksum_name": checksum_path.name,
        "relocated_root": relocated_root,
        **layout,
    }
    write_github_outputs(outputs)
    print(json.dumps({key: str(value) for key, value in outputs.items()}, indent=2))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
