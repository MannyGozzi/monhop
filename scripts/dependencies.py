#!/usr/bin/env python3
"""Generate deterministic Cargo dependency distribution reports.

The generated files describe Cargo packages only. Native DLL imports depend on a
built Windows executable and are intentionally not inferred or reported here.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
from typing import Callable, Iterable, Mapping, Optional, Sequence


LOCAL_SOURCE = "MonHop original source"
VENDORED_SOURCE = "Vendored third-party source (modified)"
SUPPLEMENT_DIRECTORY = Path(__file__).resolve().parents[1] / "docs" / "dependencies" / "license-supplements"
SUPPLEMENT_PROVENANCE = "provenance.json"
TARGETS = ("x86_64-pc-windows-msvc", "aarch64-apple-darwin")
REPORT_FILENAMES = (
    "license-report.json",
    "sbom.cdx.json",
    "LICENSES.md",
    "THIRD_PARTY_NOTICES.txt",
    *(f"runtime-{target}.txt" for target in TARGETS),
)
LICENSE_NAME = re.compile(r"^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)([._-].*)?$", re.IGNORECASE)
TREE_PACKAGE = re.compile(r"^(\S+) v(\S+)")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
SHA1 = re.compile(r"^[0-9a-f]{40}$")
SUPPLEMENT_FILENAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
CargoRunner = Callable[[Sequence[str], Path], str]
SUPPLEMENT_FIELDS = frozenset(
    {
        "name",
        "version",
        "repository",
        "vcs_sha1",
        "path_in_vcs",
        "license",
        "archive_sha256",
        "supplement_path",
        "license_source_url",
        "license_sha256",
    }
)


class GenerationError(RuntimeError):
    """Raised when the locked dependency data cannot produce distribution reports."""


def run_cargo(arguments: Sequence[str], workspace: Path) -> str:
    """Run Cargo without changing the lockfile or fetching an update."""
    result = subprocess.run(
        ["cargo", *arguments],
        cwd=workspace,
        capture_output=True,
        check=False,
        text=True,
        encoding="utf-8",
    )
    if result.returncode:
        raise GenerationError(f"cargo {arguments[0]} failed while reading locked dependency data")
    return result.stdout


def package_ref(package: Mapping[str, object]) -> str:
    return f"{package['name']}@{package['version']}"


def package_sort_key(package: Mapping[str, object]) -> tuple[str, str, str]:
    return (str(package["name"]), str(package["version"]), str(package["id"]))


def sorted_packages(metadata: Mapping[str, object]) -> list[Mapping[str, object]]:
    packages = metadata.get("packages")
    if not isinstance(packages, list):
        raise GenerationError("cargo metadata did not contain packages")
    try:
        return sorted(packages, key=package_sort_key)
    except (KeyError, TypeError) as error:
        raise GenerationError("cargo metadata package is incomplete") from error


def workspace_member_ids(metadata: Mapping[str, object], packages: Iterable[Mapping[str, object]]) -> set[str]:
    members = metadata.get("workspace_members")
    if not isinstance(members, list) or any(not isinstance(member, str) or not member for member in members):
        raise GenerationError("cargo metadata did not contain valid workspace members")
    package_ids: set[str] = set()
    for package in packages:
        package_id = package.get("id")
        if not isinstance(package_id, str) or not package_id:
            raise GenerationError("cargo metadata package has an invalid id")
        package_ids.add(package_id)
    if not set(members).issubset(package_ids):
        raise GenerationError("cargo metadata workspace members reference an unknown package")
    return set(members)


def source_label(package: Mapping[str, object], workspace_members: set[str]) -> str:
    source = package.get("source")
    if source is None:
        return LOCAL_SOURCE if str(package["id"]) in workspace_members else VENDORED_SOURCE
    if not isinstance(source, str) or not source:
        raise GenerationError(f"cargo metadata has an invalid source for {package_ref(package)}")
    return source


def license_rows(packages: Iterable[Mapping[str, object]], workspace_members: set[str]) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    for package in packages:
        rows.append(
            {
                "name": package["name"],
                "version": package["version"],
                "license": package.get("license"),
                "source": source_label(package, workspace_members),
                "repository": package.get("repository"),
            }
        )
    return rows


def cyclone_dx(
    metadata: Mapping[str, object], packages: list[Mapping[str, object]], workspace_members: set[str]
) -> dict[str, object]:
    package_lookup = {str(package["id"]): package_ref(package) for package in packages}
    components: list[dict[str, object]] = []
    for package in packages:
        component: dict[str, object] = {
            "type": "library",
            "name": package["name"],
            "version": package["version"],
            "bom-ref": package_ref(package),
        }
        if package.get("source"):
            component["purl"] = f"pkg:cargo/{package['name']}@{package['version']}"
        else:
            component["properties"] = [
                {"name": "monhop:source", "value": source_label(package, workspace_members)}
            ]
        if package.get("license"):
            component["licenses"] = [
                {"expression": str(package["license"]).replace("MIT/Apache-2.0", "MIT OR Apache-2.0")}
            ]
        components.append(component)

    resolve = metadata.get("resolve")
    if not isinstance(resolve, Mapping):
        raise GenerationError("cargo metadata did not contain the resolved dependency graph")
    nodes = resolve.get("nodes")
    if not isinstance(nodes, list):
        raise GenerationError("cargo metadata did not contain resolved dependency nodes")

    links: list[dict[str, object]] = []
    for node in nodes:
        if not isinstance(node, Mapping):
            raise GenerationError("cargo metadata dependency node is incomplete")
        try:
            dependencies = sorted(package_lookup[str(dependency)] for dependency in node["dependencies"])
            links.append({"ref": package_lookup[str(node["id"])], "dependsOn": dependencies})
        except (KeyError, TypeError) as error:
            raise GenerationError("cargo metadata dependency node references an unknown package") from error
    links.sort(key=lambda link: str(link["ref"]))

    # Metadata is intentionally empty: a generation timestamp makes a stable lockfile churn.
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "version": 1,
        "metadata": {},
        "components": components,
        "dependencies": links,
    }


def package_ref_from_tree_line(line: str) -> Optional[str]:
    match = TREE_PACKAGE.match(line)
    if match is None:
        return None
    return f"{match.group(1)}@{match.group(2)}"


def normalize_runtime_line(line: str, local_source_labels: Mapping[str, str]) -> str:
    """Replace a local package path after separating Cargo's package and license fields."""
    package_field, delimiter, license_field = line.partition("\t")
    if not delimiter:
        raise GenerationError("cargo tree runtime row has no package-license delimiter")

    reference = package_ref_from_tree_line(package_field)
    source = local_source_labels.get(reference)
    if source is not None:
        match = TREE_PACKAGE.match(package_field)
        assert match is not None
        package_field = f"{match.group(0)} ({source})"
    return f"{package_field} {license_field}" if license_field else package_field


def runtime_inventory(tree_output: str, local_source_labels: Mapping[str, str]) -> str:
    lines = {
        normalize_runtime_line(line.strip(), local_source_labels)
        for line in tree_output.splitlines()
        if line.strip()
    }
    return "\n".join(sorted(lines)) + ("\n" if lines else "")


def resolved_nodes(
    metadata: Mapping[str, object], package_by_id: Mapping[str, Mapping[str, object]]
) -> dict[str, Mapping[str, object]]:
    resolve = metadata.get("resolve")
    if not isinstance(resolve, Mapping):
        raise GenerationError("cargo metadata did not contain the resolved dependency graph")
    nodes = resolve.get("nodes")
    if not isinstance(nodes, list):
        raise GenerationError("cargo metadata did not contain resolved dependency nodes")

    node_by_id: dict[str, Mapping[str, object]] = {}
    for node in nodes:
        if not isinstance(node, Mapping):
            raise GenerationError("cargo metadata dependency node is incomplete")
        node_id = node.get("id")
        if not isinstance(node_id, str) or not node_id or node_id not in package_by_id:
            raise GenerationError("cargo metadata dependency node references an unknown package")
        if node_id in node_by_id:
            raise GenerationError("cargo metadata contains duplicate dependency nodes")
        node_by_id[node_id] = node

    for node in node_by_id.values():
        dependencies = node.get("dependencies")
        deps = node.get("deps")
        if not isinstance(dependencies, list) or not isinstance(deps, list):
            raise GenerationError("cargo metadata dependency node is incomplete")
        for dependency_id in dependencies:
            if not isinstance(dependency_id, str) or dependency_id not in node_by_id:
                raise GenerationError("cargo metadata dependency node references an unknown package")
        for dependency in deps:
            if not isinstance(dependency, Mapping):
                raise GenerationError("cargo metadata dependency edge is incomplete")
            dependency_id = dependency.get("pkg")
            kinds = dependency.get("dep_kinds")
            if (
                not isinstance(dependency.get("name"), str)
                or not dependency["name"]
                or not isinstance(dependency_id, str)
                or dependency_id not in node_by_id
                or not isinstance(kinds, list)
                or not kinds
            ):
                raise GenerationError("cargo metadata dependency edge is incomplete")
            for kind in kinds:
                if not isinstance(kind, Mapping) or "kind" not in kind or kind["kind"] not in (None, "build", "dev"):
                    raise GenerationError("cargo metadata dependency edge has an unknown kind")
        if set(dependencies) != {dependency["pkg"] for dependency in deps}:
            raise GenerationError("cargo metadata dependency edge lists disagree")
    return node_by_id


def supported_dependency_ids(node: Mapping[str, object]) -> list[str]:
    dependencies: set[str] = set()
    for dependency in node["deps"]:
        assert isinstance(dependency, Mapping)
        kinds = dependency["dep_kinds"]
        assert isinstance(kinds, list)
        if any(kind["kind"] in (None, "build") for kind in kinds):
            dependency_id = dependency["pkg"]
            assert isinstance(dependency_id, str)
            dependencies.add(dependency_id)
    return sorted(dependencies)


def conservative_notice_refs(target_metadatas: Iterable[Mapping[str, object]]) -> set[str]:
    """Union supported-target normal/build metadata graphs without evaluating Cargo cfgs."""
    refs: set[str] = set()
    for metadata in target_metadatas:
        packages = sorted_packages(metadata)
        members = workspace_member_ids(metadata, packages)
        package_by_id: dict[str, Mapping[str, object]] = {}
        for package in packages:
            package_id = package.get("id")
            if not isinstance(package_id, str) or not package_id:
                raise GenerationError("cargo metadata package has an invalid id")
            if package_id in package_by_id:
                raise GenerationError("cargo metadata contains duplicate package ids")
            package_by_id[package_id] = package
        node_by_id = resolved_nodes(metadata, package_by_id)
        if not members.issubset(node_by_id):
            raise GenerationError("cargo metadata workspace members have no resolved dependency node")

        pending = list(sorted(members, reverse=True))
        seen: set[str] = set()
        while pending:
            package_id = pending.pop()
            if package_id in seen:
                continue
            seen.add(package_id)
            refs.add(package_ref(package_by_id[package_id]))
            for dependency_id in reversed(supported_dependency_ids(node_by_id[package_id])):
                if dependency_id not in seen:
                    pending.append(dependency_id)
    return refs


def crate_directory(package: Mapping[str, object]) -> Path:
    manifest_path = package.get("manifest_path")
    if not isinstance(manifest_path, str):
        raise GenerationError(f"cargo metadata has no manifest path for {package_ref(package)}")
    return Path(manifest_path).parent


def native_license_files(package: Mapping[str, object]) -> list[tuple[str, Path]]:
    directory = crate_directory(package)
    files = [
        (candidate.relative_to(directory).as_posix(), candidate)
        for candidate in directory.rglob("*")
        if candidate.is_file() and LICENSE_NAME.fullmatch(candidate.name)
    ]
    files.sort(key=lambda item: item[0])
    return files


def supplement_value(record: Mapping[str, object], key: str, reference: str) -> str:
    value = record.get(key)
    if not isinstance(value, str) or not value:
        raise GenerationError(f"license supplement manifest is invalid for {reference}")
    return value


def load_license_supplements() -> dict[str, Mapping[str, object]]:
    manifest_path = SUPPLEMENT_DIRECTORY / SUPPLEMENT_PROVENANCE
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError, json.JSONDecodeError) as error:
        raise GenerationError("license supplement provenance manifest is unavailable") from error
    if not isinstance(manifest, Mapping) or set(manifest) != {"version", "supplements"}:
        raise GenerationError("license supplement provenance manifest is invalid")
    records = manifest.get("supplements")
    if manifest.get("version") != 1 or not isinstance(records, list):
        raise GenerationError("license supplement provenance manifest is invalid")

    supplements: dict[str, Mapping[str, object]] = {}
    for record in records:
        if not isinstance(record, Mapping) or set(record) != SUPPLEMENT_FIELDS:
            raise GenerationError("license supplement manifest entry is invalid")
        name = supplement_value(record, "name", "entry")
        version = supplement_value(record, "version", f"{name} supplement")
        reference = f"{name}@{version}"
        repository = supplement_value(record, "repository", reference)
        supplement_value(record, "license", reference)
        source_url = supplement_value(record, "license_source_url", reference)
        supplement_path = supplement_value(record, "supplement_path", reference)
        if (
            not SHA1.fullmatch(supplement_value(record, "vcs_sha1", reference))
            or not SHA256.fullmatch(supplement_value(record, "archive_sha256", reference))
            or not SHA256.fullmatch(supplement_value(record, "license_sha256", reference))
            or not SUPPLEMENT_FILENAME.fullmatch(supplement_path)
            or not repository.startswith("https://")
            or not source_url.startswith("https://")
            or any(character.isspace() for character in source_url)
            or "?" in source_url
            or "#" in source_url
        ):
            raise GenerationError(f"license supplement manifest is invalid for {reference}")
        path_in_vcs = record.get("path_in_vcs")
        # Cargo records a repository-root crate as an empty path, distinct from missing metadata.
        if path_in_vcs is not None and not isinstance(path_in_vcs, str):
            raise GenerationError(f"license supplement manifest is invalid for {reference}")
        if reference in supplements:
            raise GenerationError(f"license supplement manifest duplicates {reference}")
        try:
            contents = (SUPPLEMENT_DIRECTORY / supplement_path).read_bytes()
        except (FileNotFoundError, OSError) as error:
            raise GenerationError(f"license supplement file is unavailable for {reference}") from error
        if hashlib.sha256(contents).hexdigest() != record["license_sha256"]:
            raise GenerationError(f"license supplement hash mismatch for {reference}")
        supplements[reference] = record
    return supplements


def supplemental_license_file(
    package: Mapping[str, object], supplements: Mapping[str, Mapping[str, object]]
) -> Optional[tuple[str, Path]]:
    reference = package_ref(package)
    record = supplements.get(reference)
    if record is None:
        return None
    source = package.get("source")
    if not isinstance(source, str) or not source.startswith("registry+"):
        raise GenerationError(f"license supplement provenance did not match {reference}")
    if package.get("repository") != record["repository"] or package.get("license") != record["license"]:
        raise GenerationError(f"license supplement provenance did not match {reference}")
    try:
        vcs = json.loads((crate_directory(package) / ".cargo_vcs_info.json").read_text(encoding="utf-8"))
        git = vcs["git"]
        sha1 = git["sha1"]
    except (FileNotFoundError, OSError, json.JSONDecodeError, KeyError, TypeError) as error:
        raise GenerationError(f"license supplement provenance did not match {reference}") from error
    if sha1 != record["vcs_sha1"] or vcs.get("path_in_vcs") != record["path_in_vcs"]:
        raise GenerationError(f"license supplement provenance did not match {reference}")
    path = SUPPLEMENT_DIRECTORY / str(record["supplement_path"])
    return (f"license supplement: {path.name}", path)


def license_files(
    package: Mapping[str, object], supplements: Mapping[str, Mapping[str, object]]
) -> list[tuple[str, Path]]:
    files = native_license_files(package)
    if files:
        return files
    supplement = supplemental_license_file(package, supplements)
    if supplement is not None:
        return [supplement]
    raise GenerationError(f"no bundled license text for {package_ref(package)}")


def read_license_text(path: Path) -> str:
    contents = path.read_bytes()
    try:
        text = contents.decode("utf-8")
    except UnicodeDecodeError:
        text = contents.decode("utf-8", errors="replace")
    # Match the repository's LF checkout policy without changing license wording.
    return text.replace("\r\n", "\n").replace("\r", "\n")


def third_party_notices(
    packages: list[Mapping[str, object]],
    included_refs: set[str],
    workspace_members: set[str],
    supplements: Mapping[str, Mapping[str, object]],
) -> str:
    sections = [
        "MonHop third-party license texts. Generated from conservative supported-target normal/build resolved coverage. "
        "Includes inactive optional-feature packages and is not runtime or shipping proof. Original MonHop code is licensed under GPL-3.0-or-later."
    ]
    for package in packages:
        source = source_label(package, workspace_members)
        if (
            (package.get("source") is None and source == LOCAL_SOURCE)
            or package_ref(package) not in included_refs
        ):
            continue
        source_suffix = f" ({source})" if package.get("source") is None else ""
        for relative_path, license_path in license_files(package, supplements):
            sections.append(
                f"===== {package['name']} {package['version']}{source_suffix} / {relative_path} =====\n\n"
                f"{read_license_text(license_path).rstrip()}"
            )
    return "\n\n".join(sections) + "\n"


def licenses_markdown(rows: Iterable[Mapping[str, object]]) -> str:
    lines = [
        "# Dependency license report",
        "",
        "Generated from Cargo.lock. Includes all resolved target and build dependencies. Runtime-only inventories are adjacent files. "
        "Run cargo deny for the enforceable license/source policy.",
        "",
        "| Package | Version | License |",
        "|---|---|---|",
    ]
    for row in rows:
        license_expression = row["license"] or ""
        lines.append(f"| {row['name']} | {row['version']} | {license_expression} |")
    return "\n".join(lines) + "\n"


def render_json(value: object) -> str:
    return json.dumps(value, indent=2, ensure_ascii=False) + "\n"


def generate_reports(
    metadata: Mapping[str, object], runtime_trees: Mapping[str, str], notice_refs: set[str]
) -> dict[str, str]:
    """Build all report contents before the caller writes any file."""
    packages = sorted_packages(metadata)
    workspace_members = workspace_member_ids(metadata, packages)
    if not notice_refs.issubset({package_ref(package) for package in packages}):
        raise GenerationError("notice graph references an unknown locked package")
    supplements = load_license_supplements()
    rows = license_rows(packages, workspace_members)
    local_source_labels = {
        package_ref(package): source_label(package, workspace_members)
        for package in packages
        if package.get("source") is None
    }

    reports = {
        "license-report.json": render_json(rows),
        "sbom.cdx.json": render_json(cyclone_dx(metadata, packages, workspace_members)),
        "LICENSES.md": licenses_markdown(rows),
        "THIRD_PARTY_NOTICES.txt": third_party_notices(packages, notice_refs, workspace_members, supplements),
    }
    for target in TARGETS:
        try:
            reports[f"runtime-{target}.txt"] = runtime_inventory(runtime_trees[target], local_source_labels)
        except KeyError as error:
            raise GenerationError(f"missing runtime inventory for {target}") from error
    return reports


def parse_metadata(metadata_output: str) -> Mapping[str, object]:
    try:
        metadata = json.loads(metadata_output)
    except json.JSONDecodeError as error:
        raise GenerationError("cargo metadata returned invalid JSON") from error
    if not isinstance(metadata, Mapping):
        raise GenerationError("cargo metadata returned an invalid document")
    return metadata


def resolve_reports(workspace: Path, cargo_runner: CargoRunner = run_cargo) -> tuple[dict[str, str], int]:
    metadata = parse_metadata(cargo_runner(("metadata", "--locked", "--format-version", "1"), workspace))
    runtime_trees: dict[str, str] = {}
    target_metadatas: list[Mapping[str, object]] = []
    for target in TARGETS:
        runtime_trees[target] = cargo_runner(
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
            workspace,
        )
        target_metadatas.append(
            parse_metadata(
                cargo_runner(
                    ("metadata", "--locked", "--format-version", "1", "--filter-platform", target), workspace
                )
            )
        )
    reports = generate_reports(metadata, runtime_trees, conservative_notice_refs(target_metadatas))
    return reports, len(sorted_packages(metadata))


def write_reports(output_directory: Path, reports: Mapping[str, str]) -> None:
    output_directory.mkdir(parents=True, exist_ok=True)
    for filename in REPORT_FILENAMES:
        with (output_directory / filename).open("w", encoding="utf-8", newline="\n") as report:
            report.write(reports[filename])


def changed_reports(output_directory: Path, reports: Mapping[str, str]) -> list[str]:
    changed: list[str] = []
    for filename in REPORT_FILENAMES:
        path = output_directory / filename
        try:
            current = path.read_bytes().decode("utf-8")
        except FileNotFoundError:
            changed.append(filename)
            continue
        if current != reports[filename]:
            changed.append(filename)
    return changed


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if reports differ without writing files")
    parser.add_argument("--output-dir", type=Path, help="directory for generated reports")
    parser.add_argument("--workspace", type=Path, default=Path(__file__).resolve().parents[1], help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    workspace = args.workspace.resolve()
    output_directory = (args.output_dir or workspace / "docs" / "dependencies").resolve()
    try:
        reports, package_count = resolve_reports(workspace)
    except GenerationError as error:
        print(f"dependency report generation failed: {error}", file=sys.stderr)
        return 1

    if args.check:
        changed = changed_reports(output_directory, reports)
        if changed:
            print(f"dependency reports are missing or outdated: {', '.join(changed)}", file=sys.stderr)
            return 1
        print(f"Dependency reports are current for {package_count} resolved packages.")
        return 0

    write_reports(output_directory, reports)
    print(f"Wrote deterministic dependency reports for {package_count} resolved packages.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
