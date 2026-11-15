import hashlib
import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("release", Path(__file__).resolve().parents[1] / "release.py")
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def test_tag_must_match_workspace_version(self):
        with tempfile.TemporaryDirectory() as folder:
            manifest = Path(folder) / "Cargo.toml"
            manifest.write_text('[workspace.package]\nversion = "0.1.2"\n[profile.release]\nstrip = true\n')
            self.assertEqual(release.verify_tag("v0.1.2", manifest), "0.1.2")
            for tag in ("v0.1.3", "main", "0.1.2", "v0.1.2\nmalicious=output", "v0.1.2-rc1"):
                with self.subTest(tag=tag), self.assertRaises(ValueError):
                    release.verify_tag(tag, manifest)

    def test_manifest_requires_all_three_nonempty_assets(self):
        with tempfile.TemporaryDirectory() as folder:
            output = Path(folder)
            names = release.asset_names("0.1.2")
            for name in names[:2]:
                (output / name).write_bytes(b"application")
            with self.assertRaises(ValueError):
                release.write_manifest("0.1.2", "xchange/sync-ssh", output)
            (output / names[2]).touch()
            with self.assertRaises(ValueError):
                release.write_manifest("0.1.2", "xchange/sync-ssh", output)
            self.assertFalse((output / "SHA256SUMS.txt").exists())

    def test_manifest_hashes_exact_asset_bytes_and_links_downloads(self):
        with tempfile.TemporaryDirectory() as folder:
            output = Path(folder)
            names = release.asset_names("0.1.2")
            expected = []
            for index, name in enumerate(names):
                content = bytes([index]) * (1024 * 1024 + 13)
                (output / name).write_bytes(content)
                expected.append("{}  {}\n".format(hashlib.sha256(content).hexdigest(), name))
            release.write_manifest("0.1.2", "xchange/sync-ssh", output)
            self.assertEqual((output / "SHA256SUMS.txt").read_text(), "".join(expected))
            notes = (output / "RELEASE_NOTES.md").read_text()
            for name in names:
                self.assertIn("https://github.com/xchange/sync-ssh/releases/download/v0.1.2/" + name, notes)
            release.write_manifest("0.1.2", "xchange/sync-ssh", output)
            (output / "SyncSSH-old.exe").write_bytes(b"old")
            with self.assertRaises(ValueError):
                release.write_manifest("0.1.2", "xchange/sync-ssh", output)

    def test_windows_architecture_and_truncated_headers(self):
        with tempfile.TemporaryDirectory() as folder:
            binary = Path(folder) / "SyncSSH.exe"
            content = bytearray(128)
            content[:2] = b"MZ"
            struct.pack_into("<I", content, 60, 64)
            struct.pack_into("<4sH", content, 64, b"PE\0\0", 0x8664)
            binary.write_bytes(content)
            release.verify_windows_binary(binary)
            struct.pack_into("<H", content, 68, 0x14C)
            binary.write_bytes(content)
            with self.assertRaises(ValueError):
                release.verify_windows_binary(binary)
            binary.write_bytes(b"MZ")
            with self.assertRaises(ValueError):
                release.verify_windows_binary(binary)


if __name__ == "__main__":
    unittest.main()
