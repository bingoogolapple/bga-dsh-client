"""Stage the expected installers and validate downloaded release artifacts."""

import argparse
import shutil
import subprocess
from pathlib import Path


def platform_files(platform, version):
    if platform in ("macos_arm", "macos_intel"):
        dmg_arch, zip_arch = ("aarch64", "aarch64") if platform == "macos_arm" else ("x64", "x86_64")
        return {
            variant: (
                f"DeepSeekHarness_{version}_{dmg_arch}{suffix}.dmg",
                f"DeepSeek-Harness{suffix}-{zip_arch}-v{version}.zip",
            )
            for variant, suffix in (("plain", ""), ("bundled", "-bundled"))
        }
    if platform == "linux":
        stem, extension = f"DeepSeekHarness_{version}_amd64", "deb"
    elif platform == "windows":
        stem, extension = f"DeepSeekHarness_{version}_x64-setup", "exe"
    else:
        raise ValueError(f"Unknown platform: {platform}")
    return {"plain": (f"{stem}.{extension}",), "bundled": (f"{stem}-bundled.{extension}",)}


def expected_names(version):
    return {
        name
        for platform in ("macos_arm", "macos_intel", "linux", "windows")
        for names in platform_files(platform, version).values()
        for name in names
    }


def validate(directory, expected):
    entries = {path.name: path for path in directory.iterdir()}
    missing = expected - entries.keys()
    extra = entries.keys() - expected
    invalid = [name for name, path in entries.items() if path.is_symlink()
               or not path.is_file() or path.stat().st_size == 0]
    if missing or extra or invalid:
        raise ValueError(f"Invalid release files: missing={sorted(missing)}, "
                         f"extra={sorted(extra)}, invalid={sorted(invalid)}")
    for name in sorted(entries):
        print(f"{name} ({entries[name].stat().st_size} bytes)")


def stage(platform, version, directory):
    directory.mkdir(parents=True, exist_ok=False)
    expected = set()
    for variant, names in platform_files(platform, version).items():
        source = Path("dist/release") / variant
        for name in names:
            destination = directory / name
            if name.endswith(".zip"):
                subprocess.run(
                    ["ditto", "-c", "-k", "--keepParent", source / "DeepSeekHarness.app", destination],
                    check=True,
                )
            else:
                shutil.copyfile(source / name, destination)
            expected.add(name)
    validate(directory, expected)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    staging = commands.add_parser("stage")
    staging.add_argument("platform", choices=["macos_arm", "macos_intel", "linux", "windows"])
    staging.add_argument("version")
    staging.add_argument("directory", type=Path)
    checking = commands.add_parser("verify")
    checking.add_argument("version")
    checking.add_argument("directory", type=Path)
    args = parser.parse_args()
    if args.command == "stage":
        stage(args.platform, args.version, args.directory)
    else:
        validate(args.directory, expected_names(args.version))


if __name__ == "__main__":
    main()
