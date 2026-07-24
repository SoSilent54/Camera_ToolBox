#!/usr/bin/env python3
"""Regression coverage for the FFmpeg dependency consumer."""

from __future__ import annotations

from contextlib import ExitStack

import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("ffmpeg_dependency.py")
SPEC = importlib.util.spec_from_file_location("ffmpeg_dependency", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
ffmpeg_dependency = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ffmpeg_dependency)


class DependencyCacheRootTest(unittest.TestCase):
    def test_cache_root_changes_with_dependency_revision(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            project_root = Path(temporary)
            with ExitStack() as patches:
                patches.enter_context(
                    patch.object(ffmpeg_dependency, "PROJECT_ROOT", project_root)
                )
                patches.enter_context(
                    patch.object(ffmpeg_dependency, "FFMPEG_VERSION", "8.1.2")
                )
                patches.enter_context(
                    patch.object(ffmpeg_dependency, "DEPENDENCY_REVISION", 1)
                )
                revision_one = ffmpeg_dependency.dependency_cache_root(
                    "windows-x86_64-msvc"
                )
            with ExitStack() as patches:
                patches.enter_context(
                    patch.object(ffmpeg_dependency, "PROJECT_ROOT", project_root)
                )
                patches.enter_context(
                    patch.object(ffmpeg_dependency, "FFMPEG_VERSION", "8.1.2")
                )
                patches.enter_context(
                    patch.object(ffmpeg_dependency, "DEPENDENCY_REVISION", 2)
                )
                revision_two = ffmpeg_dependency.dependency_cache_root(
                    "windows-x86_64-msvc"
                )

        self.assertEqual(
            revision_one,
            project_root
            / ".deps"
            / "ffmpeg"
            / "windows-x86_64-msvc"
            / "8.1.2-r1",
        )
        self.assertEqual(revision_two.name, "8.1.2-r2")
        self.assertNotEqual(revision_one, revision_two)


if __name__ == "__main__":
    unittest.main()
