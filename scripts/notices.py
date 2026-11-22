import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "Cargo.toml"
LICENSES = Path(__file__).resolve().with_name("licenses")
PACKAGE = re.compile(r"^([a-zA-Z0-9_-]+) v([^ ]+)(?: |$)")
NOTICE = re.compile(
    r"^(?:license|copying|copyright|unlicense|notice|third[-_ ]?party[-_ ]?notices)(?:$|[-_.])",
    re.IGNORECASE,
)
FALLBACK = {
    ("sqlite-vec", "0.1.9"): "sqlite-vec-0.1.9.txt",
    ("unicode-casefold", "0.2.0"): "unicode-casefold-0.2.0.txt",
}


def output(*command: str) -> str:
    return subprocess.check_output(command, cwd=ROOT, encoding="utf-8", timeout=240)


def dependencies(target: str) -> list[dict]:
    args = ("--locked", "--manifest-path", str(MANIFEST))
    graph = output(
        "cargo", "tree", *args, "--target", target, "--edges", "normal,build",
        "--prefix", "none", "--format", "{p}",
    )
    selected = set()
    for line in graph.splitlines():
        match = PACKAGE.match(line)
        if match is None:
            raise ValueError(f"Unexpected cargo tree entry {line!r}")
        selected.add(match.groups())

    metadata = json.loads(output(
        "cargo", "metadata", *args, "--filter-platform", target, "--format-version", "1",
    ))
    packages = {
        (entry["name"], entry["version"]): entry
        for entry in metadata["packages"]
    }
    missing = selected.difference(packages)
    if missing:
        raise ValueError(f"Missing Cargo metadata for {sorted(missing)}")
    return [packages[key] for key in sorted(selected)
            if Path(packages[key]["manifest_path"]) != MANIFEST]


def legal_files(package: dict) -> list[tuple[str, Path]]:
    directory = Path(package["manifest_path"]).parent
    notices = {
        path for path in directory.rglob("*")
        if path.is_file() and NOTICE.match(path.name)
    }
    if declared := package["license_file"]:
        notices.add(directory / declared)
    if notices:
        return [(path.relative_to(directory).as_posix(), path) for path in sorted(notices)]
    key = (package["name"], package["version"])
    if fallback := FALLBACK.get(key):
        return [(fallback, LICENSES / fallback)]
    raise ValueError(f"No license text found for {key[0]} {key[1]}")


def generate(destination: Path, target: str) -> None:
    packages = dependencies(target)
    version = output("rustc", "--version").strip()
    chunks = [
        f"fmem dependency notices\n\nTarget: {target}\nToolchain: {version}\n".encode(),
    ]
    for package in packages:
        source = package["source"] or package["repository"] or "local source"
        header = (
            f"===== {package['name']} {package['version']} =====\n"
            f"Source: {source}\n"
            f"License expression: {package['license'] or 'see the declared license file'}\n"
        )
        chunks.append(header.encode("utf-8"))
        for name, path in legal_files(package):
            chunks.extend((f"--- {name} ---\n\n".encode(), path.read_bytes()))
    for name in ["sqlite-public-domain.txt", "unicode-data.txt"]:
        chunks.append((LICENSES / name).read_bytes())
    sysroot = Path(output("rustc", "--print", "sysroot").strip())
    standard_library = sysroot / "share/doc/rust/COPYRIGHT-library.html"

    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(b"\n\n".join(chunks) + b"\n")
    shutil.copyfile(standard_library, destination.with_name("RUST_STDLIB_NOTICES.html"))


def main() -> None:
    parser = argparse.ArgumentParser(description="Collect notices from the locked fmem build.")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--target")
    args = parser.parse_args()
    host = output("rustc", "-vV").splitlines()
    target = args.target or next(line.removeprefix("host: ") for line in host if line.startswith("host: "))
    generate(args.output, target)


if __name__ == "__main__":
    main()
