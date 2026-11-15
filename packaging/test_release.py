#!/usr/bin/env python3
"""Portable release contract tests; no Apple tools, credentials or VM required."""
import gzip
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "packaging/mac").mkdir(parents=True)
        (self.root / "bin").mkdir()
        self.env = dict(os.environ, PATH=str(self.root / "bin") + ":" + os.environ["PATH"])

    def command(self, name, body):
        p = self.root / "bin" / name
        p.write_text("#!/bin/sh\nset -eu\n" + body + "\n")
        p.chmod(0o755)

    def app_fixture(self, compressed=False):
        shutil.copy(ROOT / "packaging/mac/app.sh", self.root / "packaging/mac/app.sh")
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.0"\n')
        (self.root / "LICENSE").write_text("test license\n")
        image = self.root / "dist/mac-image"
        image.mkdir(parents=True)
        (image / "commit").write_text("abc123\n")
        raw = b"guest filesystem" * 100
        if compressed:
            (image / "system.raw.gz").write_bytes(gzip.compress(raw))
            (image / "system.raw.gz.size").write_text(str(len(raw)) + "\n")
            (image / "system.raw.gz.commit").write_text("abc123\n")
        else:
            (image / "system.raw").write_bytes(raw)
        self.command("uname", 'case "$1" in -s) echo Darwin;; -m) echo arm64;; esac')
        self.command("git", 'case "$1" in rev-list) echo 12;; rev-parse) echo abc123;; esac')
        self.command("cargo", 'mkdir -p target/release target/Sill.iconset; touch target/release/sill-gui')
        self.command("stat", "python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_size)' \"$3\"")
        self.command("cp", 'case "$1" in -c) shift;; -Rc) shift; set -- -R "$@";; esac; exec /bin/cp "$@"')
        self.command("iconutil", 'touch "$4"')
        self.command("security", 'exit 1')
        self.command("codesign", 'printf "%s\\n" "$*" >> signing.log')
        self.command("hdiutil", 'touch dist/Sill.dmg')
        return image

    def run_app(self):
        return subprocess.run(["sh", "packaging/mac/app.sh"], cwd=self.root,
                              env=self.env, capture_output=True, text=True)

    def test_local_raw_and_ci_compressed_image_both_package(self):
        for compressed in [False, True]:
            with self.subTest(compressed=compressed):
                # Each iteration has its own image but shares the mocked tools.
                shutil.rmtree(self.root / "dist", ignore_errors=True)
                self.app_fixture(compressed)
                result = self.run_app()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue((self.root / "dist/Sill.dmg").exists())
                self.assertEqual((self.root / "dist/Sill.app/Contents/Resources/LICENSE").read_text(), "test license\n")
                self.assertIn("--sign -", (self.root / "signing.log").read_text())

    def test_stale_compressed_image_is_rejected(self):
        image = self.app_fixture(True)
        (image / "system.raw.gz.commit").write_text("old\n")
        self.assertNotEqual(self.run_app().returncode, 0)
        self.assertFalse((self.root / "dist/Sill.dmg").exists())

    def test_image_from_another_commit_is_rejected(self):
        image = self.app_fixture(True)
        self.env["SILL_IMAGE_EXPECT_COMMIT"] = "abc123"
        (image / "commit").write_text("other\n")
        (image / "system.raw.gz.commit").write_text("other\n")
        self.assertNotEqual(self.run_app().returncode, 0)

    def test_explicit_signing_identity(self):
        self.app_fixture(True)
        self.env["SILL_SIGNING_IDENTITY"] = "Test signing identity"
        self.assertEqual(self.run_app().returncode, 0)
        self.assertIn("--sign Test signing identity", (self.root / "signing.log").read_text())

    def test_qemu_transport_uses_expected_disks_and_local_ssh(self):
        shutil.copy(ROOT / "packaging/mac/image-qemu.sh", self.root / "packaging/mac/image-qemu.sh")
        work = self.root / "work"
        work.mkdir()
        for name in ["code", "vars"]:
            (self.root / name).write_bytes(b"firmware fixture")
        self.command("qemu-system-aarch64", 'printf "%s\\n" "$@" > qemu-args')
        self.command("ssh", 'printf "%s\\n" "$@" > ssh-args')
        env = dict(self.env, SILL_QEMU_FIRMWARE=str(self.root / "code"),
                   SILL_QEMU_VARS=str(self.root / "vars"), SILL_IMAGE_SSH_PORT="23456")
        result = subprocess.run(["sh", "-eu", "-c",
            'work=work; vm=; . ./packaging/mac/image-qemu.sh; boot; guest true; wait_off'],
            cwd=self.root, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        qemu = (self.root / "qemu-args").read_text()
        self.assertIn("tcg,thread=multi", qemu)
        self.assertIn("hostfwd=tcp:127.0.0.1:23456-:22", qemu)
        for disk in ["sill-system", "sill-home", "sill-state"]:
            self.assertIn("serial=" + disk, qemu)
        self.assertIn("virtconsole,chardev=console", qemu)
        self.assertIn("virtio-net-pci,netdev=net,romfile=", qemu)
        ssh = (self.root / "ssh-args").read_text()
        self.assertIn("sill@127.0.0.1", ssh)
        self.assertIn("23456", ssh)

    def test_release_tag_and_lockfile_must_match(self):
        shutil.copy(ROOT / "packaging/check-release.py", self.root / "packaging/check-release.py")
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.0"\n')
        (self.root / "crates/demo").mkdir(parents=True)
        (self.root / "crates/demo/Cargo.toml").write_text('[package]\nname = "demo"\n')
        lock = self.root / "Cargo.lock"
        lock.write_text('[[package]]\nname = "demo"\nversion = "0.1.0"\n')
        def check(ref):
            return subprocess.run(["python3", "packaging/check-release.py"], cwd=self.root,
                                  env=dict(self.env, GITHUB_REF=ref), capture_output=True).returncode
        self.assertEqual(check("refs/tags/v0.1.0"), 0)
        self.assertNotEqual(check("refs/tags/v0.2.0"), 0)
        self.assertNotEqual(check("refs/heads/main"), 0)
        lock.write_text('[[package]]\nname = "demo"\nversion = "0.0.1"\n')
        self.assertNotEqual(check("refs/tags/v0.1.0"), 0)


if __name__ == "__main__":
    unittest.main()
