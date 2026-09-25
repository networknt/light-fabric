#!/usr/bin/env python3
"""Unit tests for scripts/select-changed-apps.py."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "select_changed_apps", Path(__file__).with_name("select-changed-apps.py")
)
selector = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(selector)

# Every image package depends on light-runtime; light-workflow-runner reaches
# the Claude contract through coding-agent-runtime.
DEPENDENCIES = {
    "light-agent-worker": ("coding-agent-runtime", "light-runtime"),
    "light-workflow-runner": ("coding-agent-runtime", "light-runtime"),
}


class SelectImagesTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        names = {name for names in selector.TARGET_PACKAGES.values() for name in names}
        names |= {"light-runtime", "coding-agent-runtime"}
        packages = []
        for name in sorted(names):
            kind = "crates" if name in {"light-runtime", "coding-agent-runtime"} else "apps"
            directory = self.root / kind / name
            (directory / "src").mkdir(parents=True)
            (directory / "src" / "lib.rs").write_text("")
            packages.append({
                "id": name,
                "name": name,
                "source": None,
                "manifest_path": str(directory / "Cargo.toml"),
            })
        (self.root / "crates/coding-agent-runtime/src/lib.rs").write_text(
            'include_str!("../../../contracts/claude-code/v1/launch.json");'
        )
        nodes = [
            {
                "id": package["id"],
                "deps": [
                    {"pkg": dependency}
                    for dependency in DEPENDENCIES.get(package["id"], ("light-runtime",))
                    if package["id"] != dependency
                ],
            }
            for package in packages
        ]
        self.metadata = {"packages": packages, "resolve": {"nodes": nodes}}

    def tearDown(self) -> None:
        self.temp.cleanup()

    def select(self, *paths: str, requested: set[str] | None = None) -> list[str]:
        with contextlib.redirect_stderr(io.StringIO()):
            return selector.select_images(
                self.root,
                {self.root / path for path in paths},
                self.metadata,
                requested or set(),
            )

    def test_app_change_selects_only_that_image(self) -> None:
        self.assertEqual(self.select("apps/light-gateway/src/main.rs"), ["light-gateway"])

    def test_shared_crate_selects_every_dependent_image(self) -> None:
        self.assertEqual(
            self.select("crates/light-runtime/src/lib.rs"), list(selector.TARGET_PACKAGES)
        )

    def test_requested_set_excludes_optional_image(self) -> None:
        release = set(selector.TARGET_PACKAGES) - {"light-knowledge-worker"}
        selected = self.select("crates/light-runtime/src/lib.rs", requested=release)
        self.assertNotIn("light-knowledge-worker", selected)
        self.assertEqual(set(selected), release)

    def test_global_input_selects_all_requested_images(self) -> None:
        self.assertEqual(
            self.select("Cargo.lock", requested={"light-a2a", "light-agent"}),
            ["light-a2a", "light-agent"],
        )

    def test_contract_change_selects_reading_images(self) -> None:
        self.assertEqual(
            self.select("contracts/claude-code/v1/launch.json"), ["light-workflow-runner"]
        )

    def test_contract_without_known_reader_selects_all_images(self) -> None:
        self.assertEqual(
            self.select("contracts/unreferenced/v1/schema.json"),
            list(selector.TARGET_PACKAGES),
        )

    def test_non_image_input_selects_nothing(self) -> None:
        self.assertEqual(self.select("README.md", "scripts/test-build.sh"), [])

    def test_unknown_requested_image_fails(self) -> None:
        with self.assertRaises(selector.SelectionError):
            self.select("README.md", requested={"not-an-image"})

    def test_missing_target_package_fails(self) -> None:
        self.metadata["packages"] = [
            package for package in self.metadata["packages"] if package["name"] != "light-a2a"
        ]
        with self.assertRaises(selector.SelectionError):
            self.select("README.md")


if __name__ == "__main__":
    unittest.main()
