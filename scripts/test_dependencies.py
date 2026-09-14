import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


MODULE_PATH = Path(__file__).with_name("dependencies.py")
SPEC = importlib.util.spec_from_file_location("dependencies", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
dependencies = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(dependencies)


class DependencyReportsTest(unittest.TestCase):
    def package(self, root, name, version, source, license_expression, package_id=None):
        crate = root / f"{name}-{version}"
        crate.mkdir()
        manifest = crate / "Cargo.toml"
        manifest.write_text(f"[package]\nname = {name!r}\n", encoding="utf-8")
        return {
            "id": package_id or f"{name} {version} ({source or crate.as_uri()})",
            "name": name,
            "version": version,
            "source": source,
            "license": license_expression,
            "repository": None,
            "manifest_path": str(manifest),
        }

    def metadata(self, root):
        local = self.package(root, "z-local", "1.0.0", None, None, "local")
        alpha = self.package(
            root,
            "alpha",
            "2.0.0",
            "registry+https://example.invalid/index",
            "MIT/Apache-2.0",
            "alpha",
        )
        (Path(alpha["manifest_path"]).parent / "LICENSE-MIT").write_text("alpha license\n", encoding="utf-8")
        return {
            "packages": [local, alpha],
            "workspace_members": ["local"],
            "resolve": {
                "nodes": [
                    {
                        "id": "local",
                        "dependencies": ["alpha"],
                        "deps": [
                            {
                                "name": "alpha",
                                "pkg": "alpha",
                                "dep_kinds": [{"kind": None, "target": None}],
                            }
                        ],
                    },
                    {"id": "alpha", "dependencies": [], "deps": []},
                ]
            },
        }

    def metadata_with_vendored_package(self, root):
        metadata = self.metadata(root)
        vendored = self.package(root, "vendored", "3.0.0", None, "MIT", "vendored")
        (Path(vendored["manifest_path"]).parent / "LICENSE").write_text("vendored license\n", encoding="utf-8")
        metadata["packages"].append(vendored)
        metadata["resolve"]["nodes"].append({"id": "vendored", "dependencies": [], "deps": []})
        return metadata, vendored

    def supplement_package(self, root, version="1.0.0"):
        package = self.package(
            root,
            "supplemented",
            version,
            "registry+https://example.invalid/index",
            "MIT",
            f"supplemented-{version}",
        )
        package["repository"] = "https://example.invalid/supplemented"
        crate = Path(package["manifest_path"]).parent
        (crate / ".cargo_vcs_info.json").write_text(
            json.dumps({"git": {"sha1": "1" * 40}, "path_in_vcs": "crates/supplemented"}),
            encoding="utf-8",
        )
        return package

    def write_supplement(self, directory, package, contents=b"supplement license\n"):
        directory.mkdir(exist_ok=True)
        filename = "supplemented-LICENSE"
        (directory / filename).write_bytes(contents)
        record = {
            "name": package["name"],
            "version": package["version"],
            "repository": package["repository"],
            "vcs_sha1": "1" * 40,
            "path_in_vcs": "crates/supplemented",
            "license": package["license"],
            "archive_sha256": "2" * 64,
            "supplement_path": filename,
            "license_source_url": "https://example.invalid/licenses/supplemented",
            "license_sha256": hashlib.sha256(contents).hexdigest(),
        }
        (directory / "provenance.json").write_text(
            json.dumps({"version": 1, "supplements": [record]}), encoding="utf-8"
        )
        return directory / filename

    def reports_for_supplement_package(self, package):
        package_line = f"{package['name']} v{package['version']}\t{package['license']}\n"
        return dependencies.generate_reports(
            {
                "packages": [package],
                "workspace_members": [],
                "resolve": {"nodes": [{"id": package["id"], "dependencies": [], "deps": []}]},
            },
            {target: package_line for target in dependencies.TARGETS},
            {f"{package['name']}@{package['version']}"},
        )

    def test_reports_have_stable_sorted_metadata_and_graph(self):
        with tempfile.TemporaryDirectory() as temporary:
            metadata = self.metadata(Path(temporary))
            reports = dependencies.generate_reports(
                metadata,
                {
                    "x86_64-pc-windows-msvc": "z-local v1.0.0 (/private/work/z-local)\t<none>\nalpha v2.0.0\tMIT/Apache-2.0\n",
                    "aarch64-apple-darwin": "alpha v2.0.0\tMIT/Apache-2.0\nz-local v1.0.0 (/private/work/z-local)\t<none>\n",
                },
                {"z-local@1.0.0", "alpha@2.0.0"},
            )

        report_rows = json.loads(reports["license-report.json"])
        self.assertEqual([row["name"] for row in report_rows], ["alpha", "z-local"])
        self.assertEqual(report_rows[1]["source"], dependencies.LOCAL_SOURCE)
        sbom = json.loads(reports["sbom.cdx.json"])
        self.assertNotIn("timestamp", sbom["metadata"])
        self.assertEqual([component["name"] for component in sbom["components"]], ["alpha", "z-local"])
        self.assertEqual([link["ref"] for link in sbom["dependencies"]], ["alpha@2.0.0", "z-local@1.0.0"])
        self.assertEqual(sbom["dependencies"][1]["dependsOn"], ["alpha@2.0.0"])
        alpha_component = next(component for component in sbom["components"] if component["name"] == "alpha")
        local_component = next(component for component in sbom["components"] if component["name"] == "z-local")
        self.assertEqual(alpha_component["purl"], "pkg:cargo/alpha@2.0.0")
        self.assertNotIn("properties", alpha_component)
        self.assertEqual(
            local_component["properties"],
            [{"name": "monhop:source", "value": dependencies.LOCAL_SOURCE}],
        )
        self.assertEqual(
            reports["runtime-x86_64-pc-windows-msvc.txt"].splitlines(),
            ["alpha v2.0.0 MIT/Apache-2.0", f"z-local v1.0.0 ({dependencies.LOCAL_SOURCE}) <none>"],
        )
        self.assertIn("===== alpha 2.0.0 / LICENSE-MIT =====", reports["THIRD_PARTY_NOTICES.txt"])
        self.assertIn(
            "conservative supported-target normal/build resolved coverage", reports["THIRD_PARTY_NOTICES.txt"]
        )
        self.assertIn("inactive optional-feature packages", reports["THIRD_PARTY_NOTICES.txt"])
        self.assertNotIn("runtime-system-windows.txt", dependencies.REPORT_FILENAMES)

    def test_license_line_endings_match_clean_checkouts(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "LICENSE"
            for raw in (b"first\r\nsecond\r\n", b"first\rsecond\r", b"first\nsecond\n"):
                with self.subTest(raw=raw):
                    path.write_bytes(raw)
                    self.assertEqual(dependencies.read_license_text(path), "first\nsecond\n")

    def test_cargo_metadata_uses_utf8_on_every_host(self):
        result = mock.Mock(returncode=0, stdout='{"author":"Álvarez"}')
        with mock.patch.object(dependencies.subprocess, "run", return_value=result) as run:
            self.assertEqual(dependencies.run_cargo(("metadata",), Path(".")), result.stdout)
        self.assertEqual(run.call_args.kwargs["encoding"], "utf-8")

    def test_missing_license_text_fails_before_reports_are_written(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = self.package(root, "without-text", "1.0.0", "registry+https://example.invalid/index", "MIT", "external")
            metadata = {
                "packages": [package],
                "workspace_members": [],
                "resolve": {"nodes": [{"id": "external", "dependencies": [], "deps": []}]},
            }
            with self.assertRaisesRegex(dependencies.GenerationError, "no bundled license text"):
                dependencies.generate_reports(
                    metadata,
                    {target: "without-text v1.0.0\tMIT\n" for target in dependencies.TARGETS},
                    {"without-text@1.0.0"},
                )

    def test_license_supplement_is_verified_before_notice_generation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = self.supplement_package(root)
            supplement_directory = root / "supplements"
            self.write_supplement(supplement_directory, package)
            with mock.patch.object(dependencies, "SUPPLEMENT_DIRECTORY", supplement_directory):
                reports = self.reports_for_supplement_package(package)
            self.assertIn(
                "===== supplemented 1.0.0 / license supplement: supplemented-LICENSE =====",
                reports["THIRD_PARTY_NOTICES.txt"],
            )
            self.assertIn("supplement license", reports["THIRD_PARTY_NOTICES.txt"])
            crate = Path(package["manifest_path"]).parent
            (crate / ".cargo_vcs_info.json").write_text(
                json.dumps({"git": {"sha1": "3" * 40}, "path_in_vcs": "crates/supplemented"}),
                encoding="utf-8",
            )
            with mock.patch.object(dependencies, "SUPPLEMENT_DIRECTORY", supplement_directory):
                with self.assertRaisesRegex(dependencies.GenerationError, "provenance did not match"):
                    self.reports_for_supplement_package(package)

    def test_repository_root_license_supplement_preserves_exact_empty_path(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = self.supplement_package(root)
            supplement_directory = root / "supplements"
            self.write_supplement(supplement_directory, package)
            manifest = supplement_directory / "provenance.json"
            contents = json.loads(manifest.read_text())
            contents["supplements"][0]["path_in_vcs"] = ""
            manifest.write_text(json.dumps(contents))
            vcs = Path(package["manifest_path"]).parent / ".cargo_vcs_info.json"
            vcs.write_text(json.dumps({"git": {"sha1": "1" * 40}, "path_in_vcs": ""}))
            with mock.patch.object(dependencies, "SUPPLEMENT_DIRECTORY", supplement_directory):
                self.assertIn("supplement license", self.reports_for_supplement_package(package)["THIRD_PARTY_NOTICES.txt"])
                vcs.write_text(json.dumps({"git": {"sha1": "1" * 40}}))
                with self.assertRaisesRegex(dependencies.GenerationError, "provenance did not match"):
                    self.reports_for_supplement_package(package)

    def test_missing_changed_or_new_license_supplement_fails_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = self.supplement_package(root)
            supplement_directory = root / "supplements"
            supplement_path = self.write_supplement(supplement_directory, package)
            supplement_path.unlink()
            with mock.patch.object(dependencies, "SUPPLEMENT_DIRECTORY", supplement_directory):
                with self.assertRaisesRegex(dependencies.GenerationError, "supplement file is unavailable"):
                    self.reports_for_supplement_package(package)

            supplement_path = self.write_supplement(supplement_directory, package)
            supplement_path.write_bytes(b"changed\n")
            with mock.patch.object(dependencies, "SUPPLEMENT_DIRECTORY", supplement_directory):
                with self.assertRaisesRegex(dependencies.GenerationError, "supplement hash mismatch"):
                    self.reports_for_supplement_package(package)

            supplement_path = self.write_supplement(supplement_directory, package)
            self.assertTrue(supplement_path.exists())
            newer = self.supplement_package(root, "1.0.1")
            with mock.patch.object(dependencies, "SUPPLEMENT_DIRECTORY", supplement_directory):
                with self.assertRaisesRegex(dependencies.GenerationError, "no bundled license text"):
                    self.reports_for_supplement_package(newer)

    def test_checked_in_license_supplements_are_hashed_and_exactly_mapped(self):
        supplements = dependencies.load_license_supplements()
        self.assertEqual(len(supplements), 28)
        self.assertIn("selectors@0.36.1", supplements)
        self.assertIn("webview2-com@0.38.2", supplements)
        self.assertIn("objc2-core-location@0.3.2", supplements)
        self.assertEqual(supplements["clipboard-win@5.4.1"]["path_in_vcs"], "")

    def test_local_paths_are_not_written_to_reports(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            metadata = self.metadata(root)
            reports = dependencies.generate_reports(
                metadata,
                {target: f"z-local v1.0.0 ({root}/z-local-1.0.0)\t<none>\n" for target in dependencies.TARGETS},
                {"alpha@2.0.0"},
            )

            for contents in reports.values():
                self.assertNotIn(str(root), contents)
            self.assertIn(f"z-local v1.0.0 ({dependencies.LOCAL_SOURCE})", reports["runtime-aarch64-apple-darwin.txt"])

    def test_vendored_path_package_is_labeled_noticed_and_redacted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            metadata, vendored = self.metadata_with_vendored_package(root)
            vendor_path = Path(vendored["manifest_path"]).parent
            reports = dependencies.generate_reports(
                metadata,
                {
                    target: f"vendored v3.0.0 ({vendor_path})\tMIT\n"
                    for target in dependencies.TARGETS
                },
                {"vendored@3.0.0"},
            )

            rows = json.loads(reports["license-report.json"])
            vendor_row = next(row for row in rows if row["name"] == "vendored")
            self.assertEqual(vendor_row["source"], dependencies.VENDORED_SOURCE)
            self.assertEqual(
                reports["runtime-aarch64-apple-darwin.txt"],
                f"vendored v3.0.0 ({dependencies.VENDORED_SOURCE}) MIT\n",
            )
            self.assertIn(
                f"===== vendored 3.0.0 ({dependencies.VENDORED_SOURCE}) / LICENSE =====",
                reports["THIRD_PARTY_NOTICES.txt"],
            )
            self.assertIn("vendored license", reports["THIRD_PARTY_NOTICES.txt"])
            sbom = json.loads(reports["sbom.cdx.json"])
            vendor_component = next(component for component in sbom["components"] if component["name"] == "vendored")
            self.assertEqual(
                vendor_component["properties"],
                [{"name": "monhop:source", "value": dependencies.VENDORED_SOURCE}],
            )
            self.assertNotIn("purl", vendor_component)
            for contents in reports.values():
                self.assertNotIn(str(root), contents)

    def test_missing_or_invalid_workspace_members_fail_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            metadata = self.metadata(root)
            del metadata["workspace_members"]
            with self.assertRaisesRegex(dependencies.GenerationError, "workspace members"):
                dependencies.generate_reports(
                    metadata,
                    {target: "alpha v2.0.0\tMIT/Apache-2.0\n" for target in dependencies.TARGETS},
                    {"alpha@2.0.0"},
                )

            metadata["workspace_members"] = ["unknown"]
            with self.assertRaisesRegex(dependencies.GenerationError, "unknown package"):
                dependencies.generate_reports(
                    metadata,
                    {target: "alpha v2.0.0\tMIT/Apache-2.0\n" for target in dependencies.TARGETS},
                    {"alpha@2.0.0"},
                )

    def test_reports_remain_deterministic_when_metadata_order_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            metadata = self.metadata(root)
            runtime_trees = {
                target: f"z-local v1.0.0 ({root}/z-local-1.0.0)\t<none>\nalpha v2.0.0\tMIT/Apache-2.0\n"
                for target in dependencies.TARGETS
            }
            notices = {"z-local@1.0.0", "alpha@2.0.0"}
            first = dependencies.generate_reports(metadata, runtime_trees, notices)
            metadata["packages"].reverse()
            metadata["resolve"]["nodes"].reverse()
            second = dependencies.generate_reports(metadata, runtime_trees, notices)
            self.assertEqual(first, second)

    def test_runtime_delimiter_handles_parenthesized_local_path_and_spdx_expression(self):
        line = "z-local v1.0.0 (/tmp/MonHop (test)/apps/monhop)\tMIT OR (Apache-2.0 AND BSD-3-Clause)"
        inventory = dependencies.runtime_inventory(
            line + "\n", {"z-local@1.0.0": dependencies.LOCAL_SOURCE}
        )
        self.assertEqual(
            inventory,
            f"z-local v1.0.0 ({dependencies.LOCAL_SOURCE}) MIT OR (Apache-2.0 AND BSD-3-Clause)\n",
        )
        self.assertNotIn("/tmp/MonHop (test)", inventory)

    def test_conservative_notices_include_build_and_proc_macro_but_exclude_dev_only(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            packages = [
                self.package(root, "local", "1.0.0", None, None, "local"),
                self.package(root, "normal", "1.0.0", "registry+https://example.invalid/index", "MIT", "normal"),
                self.package(root, "build", "1.0.0", "registry+https://example.invalid/index", "MIT", "build"),
                self.package(root, "proc", "1.0.0", "registry+https://example.invalid/index", "MIT", "proc"),
                self.package(root, "dev", "1.0.0", "registry+https://example.invalid/index", "MIT", "dev"),
            ]
            packages[3]["targets"] = [{"kind": ["proc-macro"]}]
            metadata = {
                "packages": packages,
                "workspace_members": ["local"],
                "resolve": {
                    "nodes": [
                        {
                            "id": "local",
                            "dependencies": ["normal", "build", "dev"],
                            "deps": [
                                {"name": "normal", "pkg": "normal", "dep_kinds": [{"kind": "dev"}, {"kind": None}]},
                                {"name": "build", "pkg": "build", "dep_kinds": [{"kind": "build"}]},
                                {"name": "dev", "pkg": "dev", "dep_kinds": [{"kind": "dev"}]},
                            ],
                        },
                        {"id": "normal", "dependencies": [], "deps": []},
                        {
                            "id": "build",
                            "dependencies": ["proc"],
                            "deps": [{"name": "proc", "pkg": "proc", "dep_kinds": [{"kind": None}]}],
                        },
                        {"id": "proc", "dependencies": [], "deps": []},
                        {"id": "dev", "dependencies": [], "deps": []},
                    ]
                },
            }

            self.assertEqual(
                dependencies.conservative_notice_refs([metadata]),
                {"local@1.0.0", "normal@1.0.0", "build@1.0.0", "proc@1.0.0"},
            )

    def test_conservative_notices_handle_cycles_and_normalize_local_ids(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            local = self.package(root, "local", "1.0.0", None, None, "file:///tmp/MonHop%20(test)/local")
            alpha = self.package(
                root,
                "alpha",
                "2.0.0",
                "registry+https://example.invalid/index",
                "MIT",
                "alpha",
            )
            metadata = {
                "packages": [local, alpha],
                "workspace_members": [local["id"]],
                "resolve": {
                    "nodes": [
                        {
                            "id": local["id"],
                            "dependencies": ["alpha"],
                            "deps": [{"name": "alpha", "pkg": "alpha", "dep_kinds": [{"kind": None}]}],
                        },
                        {
                            "id": "alpha",
                            "dependencies": [local["id"]],
                            "deps": [
                                {"name": "local", "pkg": local["id"], "dep_kinds": [{"kind": None}]}
                            ],
                        },
                    ]
                },
            }

            refs = dependencies.conservative_notice_refs([metadata])
            self.assertEqual(refs, {"local@1.0.0", "alpha@2.0.0"})
            self.assertNotIn("/tmp/MonHop", "\n".join(refs))

    def test_conservative_notice_union_is_deterministic_across_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            local = self.package(root, "local", "1.0.0", None, None, "local")
            alpha = self.package(
                root,
                "alpha",
                "1.0.0",
                "registry+https://example.invalid/index",
                "MIT",
                "alpha",
            )
            beta = self.package(root, "beta", "1.0.0", "registry+https://example.invalid/index", "MIT", "beta")
            first = {
                "packages": [local, alpha],
                "workspace_members": ["local"],
                "resolve": {
                    "nodes": [
                        {
                            "id": "local",
                            "dependencies": ["alpha"],
                            "deps": [{"name": "alpha", "pkg": "alpha", "dep_kinds": [{"kind": None}]}],
                        },
                        {"id": "alpha", "dependencies": [], "deps": []},
                    ]
                },
            }
            second = {
                "packages": [beta, local],
                "workspace_members": ["local"],
                "resolve": {
                    "nodes": [
                        {
                            "id": "local",
                            "dependencies": ["beta"],
                            "deps": [{"name": "beta", "pkg": "beta", "dep_kinds": [{"kind": None}]}],
                        },
                        {"id": "beta", "dependencies": [], "deps": []},
                    ]
                },
            }

            expected = {"local@1.0.0", "alpha@1.0.0", "beta@1.0.0"}
            self.assertEqual(dependencies.conservative_notice_refs([first, second]), expected)
            self.assertEqual(dependencies.conservative_notice_refs([second, first]), expected)

    def test_conservative_notices_reject_invalid_graph(self):
        with tempfile.TemporaryDirectory() as temporary:
            first_root = Path(temporary) / "first"
            first_root.mkdir()
            metadata = self.metadata(first_root)
            node = metadata["resolve"]["nodes"][0]
            node["dependencies"] = ["missing"]
            node["deps"] = [{"name": "missing", "pkg": "missing", "dep_kinds": [{"kind": None}]}]
            with self.assertRaisesRegex(dependencies.GenerationError, "unknown package"):
                dependencies.conservative_notice_refs([metadata])

            second_root = Path(temporary) / "second"
            second_root.mkdir()
            metadata = self.metadata(second_root)
            metadata["resolve"]["nodes"][0]["deps"][0]["dep_kinds"] = [{"kind": "unknown"}]
            with self.assertRaisesRegex(dependencies.GenerationError, "unknown kind"):
                dependencies.conservative_notice_refs([metadata])

    def test_notice_graphs_cannot_silently_omit_inconsistent_or_unknown_packages(self):
        with tempfile.TemporaryDirectory() as temporary:
            metadata = self.metadata(Path(temporary))
            metadata["resolve"]["nodes"][0]["deps"] = []
            with self.assertRaisesRegex(dependencies.GenerationError, "edge lists disagree"):
                dependencies.conservative_notice_refs([metadata])
            with self.assertRaisesRegex(dependencies.GenerationError, "unknown locked package"):
                dependencies.generate_reports(metadata, {}, {"missing@1.0.0"})

    def test_locked_cargo_commands_and_check_mode_use_controlled_output_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            metadata = self.metadata(root)
            calls = []

            def fake_cargo(arguments, workspace):
                calls.append((tuple(arguments), workspace))
                if arguments[0] == "metadata":
                    return json.dumps(metadata)
                target = arguments[arguments.index("--target") + 1]
                self.assertIn(target, dependencies.TARGETS)
                self.assertEqual(arguments[arguments.index("--edges") + 1], "normal,no-proc-macro")
                return f"z-local v1.0.0 ({root}/z-local-1.0.0)\t<none>\n"

            reports, count = dependencies.resolve_reports(root, fake_cargo)
            self.assertEqual(count, 2)
            self.assertEqual(len(calls), 5)
            self.assertTrue(all("--locked" in arguments for arguments, _ in calls))
            tree_commands = [arguments for arguments, _ in calls if arguments[0] == "tree"]
            self.assertEqual(len(tree_commands), len(dependencies.TARGETS))
            for target in dependencies.TARGETS:
                self.assertIn(
                    (
                        "tree",
                        "--locked",
                        "--target",
                        target,
                        "--edges",
                        "normal,no-proc-macro",
                        "--no-dedupe",
                        "--prefix",
                        "none",
                        "--format",
                        "{p}\t{l}",
                    ),
                    tree_commands,
                )
                self.assertIn(
                    ("metadata", "--locked", "--format-version", "1", "--filter-platform", target),
                    [arguments for arguments, _ in calls if arguments[0] == "metadata"],
                )

            output = root / "reports"
            with mock.patch.object(dependencies, "resolve_reports", return_value=(reports, count)):
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(
                        dependencies.main(["--check", "--workspace", str(root), "--output-dir", str(output)]),
                        1,
                    )
            self.assertFalse(output.exists())

            dependencies.write_reports(output, reports)
            self.assertEqual(dependencies.changed_reports(output, reports), [])
            with mock.patch.object(dependencies, "resolve_reports", return_value=(reports, count)):
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(
                        dependencies.main(["--check", "--workspace", str(root), "--output-dir", str(output)]),
                        0,
                    )
            (output / "LICENSES.md").write_text("stale\n", encoding="utf-8")
            self.assertEqual(dependencies.changed_reports(output, reports), ["LICENSES.md"])


if __name__ == "__main__":
    unittest.main()
