#!/usr/bin/env python3
"""Validate and assemble SyncSSH release assets using only the standard library."""

import argparse
import hashlib
import os
from pathlib import Path
import plistlib
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parent.parent
VERSION_PATTERN = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"


def workspace_version(manifest=ROOT / "Cargo.toml"):
    text = manifest.read_text(encoding="utf-8")
    section = re.search(r"(?ms)^\[workspace\.package\]\s*\n(.*?)(?=^\[|\Z)", text)
    match = re.search(r'^version\s*=\s*"([^"]+)"\s*$', section[1], re.M) if section else None
    if not match or not re.fullmatch(VERSION_PATTERN, match[1]):
        raise ValueError("Expected a stable three-part version in [workspace.package]")
    return match[1]


def verify_version(version, manifest=ROOT / "Cargo.toml"):
    if not re.fullmatch(VERSION_PATTERN, version) or version != workspace_version(manifest):
        raise ValueError("Release version must match the Cargo workspace version")
    return version


def verify_tag(tag, manifest=ROOT / "Cargo.toml"):
    version = workspace_version(manifest)
    if tag != "v" + version:
        raise ValueError("Version tag must be exactly v" + version)
    return version


def asset_names(version):
    return (
        "SyncSSH-{}-macos-arm64.zip".format(version),
        "SyncSSH-{}-macos-x86_64.zip".format(version),
        "SyncSSH-{}-windows-x86_64.exe".format(version),
    )


def verify_windows_binary(binary):
    with binary.open("rb") as stream:
        header = stream.read(64)
        if len(header) != 64 or header[:2] != b"MZ":
            raise ValueError("Windows executable has no DOS header")
        offset = struct.unpack_from("<I", header, 60)[0]
        if offset < 64 or offset + 24 > binary.stat().st_size:
            raise ValueError("Windows executable has an invalid PE offset")
        stream.seek(offset)
        signature, machine = struct.unpack("<4sH", stream.read(6))
        if signature != b"PE\0\0" or machine != 0x8664:
            raise ValueError("Expected an x86_64 Windows PE executable")


def package_windows(version, binary, output):
    verify_version(version)
    verify_windows_binary(binary)
    output.mkdir(parents=True, exist_ok=True)
    destination = output / asset_names(version)[2]
    with tempfile.TemporaryDirectory(dir=output) as staging:
        temporary = Path(staging) / destination.name
        shutil.copy2(binary, temporary)
        os.replace(temporary, destination)
    return destination


def package_macos(version, arch, bundle, output):
    verify_version(version)
    with (bundle / "Contents/Info.plist").open("rb") as stream:
        info = plistlib.load(stream)
    if info.get("CFBundleShortVersionString") != version or info.get("CFBundleVersion") != version:
        raise ValueError("macOS application version does not match the release")
    executable = bundle / "Contents/MacOS/sync-ssh"
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError("macOS application executable is missing or not executable")
    if not (bundle / "Contents/Resources/SyncSSH.icns").is_file():
        raise ValueError("macOS application icon is missing")
    architectures = subprocess.check_output(["lipo", "-archs", str(executable)], text=True).split()
    if architectures != [arch]:
        raise ValueError("macOS application architecture does not match " + arch)
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle)], check=True)
    output.mkdir(parents=True, exist_ok=True)
    destination = output / "SyncSSH-{}-macos-{}.zip".format(version, arch)
    with tempfile.TemporaryDirectory(dir=output) as staging:
        temporary = Path(staging) / destination.name
        subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(bundle), str(temporary)], check=True)
        with zipfile.ZipFile(temporary) as archive:
            if archive.testzip() is not None:
                raise ValueError("macOS archive failed its integrity check")
            for entry in ("Contents/Info.plist", "Contents/MacOS/sync-ssh", "Contents/Resources/SyncSSH.icns"):
                archive.getinfo(bundle.name + "/" + entry)
        os.replace(temporary, destination)
    return destination


def write_manifest(version, repo, output):
    if not re.fullmatch(VERSION_PATTERN, version):
        raise ValueError("Invalid release version")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise ValueError("Repository must be OWNER/REPO")
    names = asset_names(version)
    actual = {item.name for item in output.iterdir() if item.suffix in (".zip", ".exe")}
    if actual != set(names):
        raise ValueError("Release requires exactly these three assets: " + ", ".join(names))
    checksums = []
    for name in names:
        asset = output / name
        if not asset.is_file() or asset.stat().st_size == 0:
            raise ValueError("Empty or missing release asset: " + name)
        digest = hashlib.sha256()
        with asset.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
        checksums.append("{}  {}\n".format(digest.hexdigest(), name))
    (output / "SHA256SUMS.txt").write_text("".join(checksums), encoding="utf-8")
    base = "https://github.com/{}/releases/download/v{}/".format(repo, version)
    labels = ("macOS Apple Silicon (M1 or later)", "macOS Intel", "Windows x64")
    lines = ["## SyncSSH " + version, "", "| Platform | Download |", "| --- | --- |"]
    lines.extend("| {} | [{}]({}{}) |".format(label, name, base, name) for label, name in zip(labels, names))
    lines.extend([
        "", "[SHA256 checksums](" + base + "SHA256SUMS.txt)", "",
        "macOS: extract the ZIP and move SyncSSH.app to Applications. Windows: run the single EXE.",
        "The macOS apps use ad-hoc signing and are not Developer ID signed or notarized. "
        "The Windows executable is not Authenticode signed.", "",
    ])
    (output / "RELEASE_NOTES.md").write_text("\n".join(lines), encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    version = commands.add_parser("version")
    version.add_argument("--tag")
    version.add_argument("--github-output", type=Path)
    for command in ("package-macos", "package-windows", "manifest"):
        child = commands.add_parser(command)
        child.add_argument("--version", required=True)
        child.add_argument("--output", type=Path, default=ROOT / "artifacts")
        if command == "package-macos":
            child.add_argument("--arch", choices=("arm64", "x86_64"), required=True)
            child.add_argument("--bundle", type=Path, default=ROOT / "dist/SyncSSH.app")
        elif command == "package-windows":
            child.add_argument("--binary", type=Path, default=ROOT / "dist/SyncSSH.exe")
        else:
            child.add_argument("--repo", required=True)
    args = parser.parse_args()
    try:
        if args.command == "version":
            version = verify_tag(args.tag) if args.tag is not None else workspace_version()
            if args.github_output:
                with args.github_output.open("a", encoding="utf-8") as stream:
                    stream.write("version={}\ntag=v{}\n".format(version, version))
            print(version)
        elif args.command == "package-macos":
            print(package_macos(args.version, args.arch, args.bundle, args.output))
        elif args.command == "package-windows":
            print(package_windows(args.version, args.binary, args.output))
        else:
            verify_version(args.version)
            write_manifest(args.version, args.repo, args.output)
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError, zipfile.BadZipFile) as error:
        print("Release error: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
