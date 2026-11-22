"""Упаковать бинарник и уведомления без моделей и локального состояния."""

from pathlib import Path
import re
import sys
import zipfile


def main():
    target = sys.argv[1]
    if not re.fullmatch(r"[a-z0-9_-]+", target):
        raise ValueError("Некорректная платформа")
    archive = Path(f"fmem-{target}.zip")
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
        for path in sorted(Path("dist").rglob("*")):
            if path.is_file():
                output.write(path, path.relative_to("dist"))
    print(archive)


if __name__ == "__main__":
    main()
