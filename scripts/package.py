"""Упаковать бинарник и уведомления без моделей и локального состояния."""

from pathlib import Path
import re
import sys
import zipfile

PLUGIN_FILES = (
    ".agents/plugins/marketplace.json",
    ".codex-plugin/plugin.json",
    "hooks/hooks.json",
    "hooks/memory.ps1",
    "hooks/memory.sh",
)


def main():
    target = sys.argv[1]
    if not re.fullmatch(r"[a-z0-9_-]+", target):
        raise ValueError("Некорректная платформа")
    archive = Path(f"fmem-{target}.zip")
    root = Path(__file__).resolve().parents[1]
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
        for path in sorted(Path("dist").rglob("*")):
            if path.is_file():
                output.write(path, path.relative_to("dist"))
        for name in PLUGIN_FILES:
            output.write(root / name, name)
        for path in sorted((root / "docs").rglob("*")):
            if path.is_file():
                output.write(path, path.relative_to(root))
    print(archive)


if __name__ == "__main__":
    main()
