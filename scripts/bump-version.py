import pathlib
import re
import sys

PACKAGE = "sonn-client"


def update_cargo_toml(version: str) -> None:
    path = pathlib.Path("Cargo.toml")
    text = path.read_text(encoding="utf-8")
    updated, count = re.subn(
        r'(?m)^version = "([^"]+)"$',
        f'version = "{version}"',
        text,
        count=1,
    )
    if count == 0:
        raise SystemExit("Cargo.toml version not found")
    path.write_text(updated, encoding="utf-8")


def update_cargo_lock(version: str) -> None:
    path = pathlib.Path("Cargo.lock")
    if not path.exists():
        return
    text = path.read_text(encoding="utf-8")
    pattern = re.compile(
        r'(?ms)(\[\[package\]\]\n(?:[^\n]*\n)*?name = "'
        + re.escape(PACKAGE)
        + r'"\n(?:[^\n]*\n)*?version = ")([^"]+)(")'
    )
    updated, count = pattern.subn(rf"\g<1>{version}\3", text, count=1)
    if count == 0:
        raise SystemExit("Cargo.lock package version not found")
    path.write_text(updated, encoding="utf-8")


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: bump-version.py <version>")
    version = sys.argv[1]
    update_cargo_toml(version)
    update_cargo_lock(version)


if __name__ == "__main__":
    main()
