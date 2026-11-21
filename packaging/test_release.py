#!/usr/bin/env python3
"""Portable release contract tests; no Apple tools, credentials or VM required."""
import gzip
import hashlib
import io
import os
from pathlib import Path
import pty
import shutil
import subprocess
import tarfile
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
        self.command("cargo", 'mkdir -p target/release target/Silli.iconset; touch target/release/silli-gui')
        self.command("stat", "python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_size)' \"$3\"")
        self.command("cp", 'case "$1" in -c) shift;; -Rc) shift; set -- -R "$@";; esac; exec /bin/cp "$@"')
        self.command("iconutil", 'touch "$4"')
        self.command("security", 'exit 1')
        self.command("codesign", 'printf "%s\\n" "$*" >> signing.log')
        self.command("hdiutil", 'touch dist/Silli.dmg')
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
                self.assertTrue((self.root / "dist/Silli.dmg").exists())
                self.assertEqual((self.root / "dist/Silli.app/Contents/Resources/LICENSE").read_text(), "test license\n")
                self.assertIn("--sign -", (self.root / "signing.log").read_text())

    def test_stale_compressed_image_is_rejected(self):
        image = self.app_fixture(True)
        (image / "system.raw.gz.commit").write_text("old\n")
        self.assertNotEqual(self.run_app().returncode, 0)
        self.assertFalse((self.root / "dist/Silli.dmg").exists())

    def test_image_from_another_commit_is_rejected(self):
        image = self.app_fixture(True)
        self.env["SILLI_IMAGE_EXPECT_COMMIT"] = "abc123"
        (image / "commit").write_text("other\n")
        (image / "system.raw.gz.commit").write_text("other\n")
        self.assertNotEqual(self.run_app().returncode, 0)

    def test_explicit_signing_identity(self):
        self.app_fixture(True)
        self.env["SILLI_SIGNING_IDENTITY"] = "Test signing identity"
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
        env = dict(self.env, SILLI_QEMU_FIRMWARE=str(self.root / "code"),
                   SILLI_QEMU_VARS=str(self.root / "vars"), SILLI_IMAGE_SSH_PORT="23456")
        result = subprocess.run(["sh", "-eu", "-c",
            'work=work; vm=; . ./packaging/mac/image-qemu.sh; boot; guest true; wait_off'],
            cwd=self.root, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        qemu = (self.root / "qemu-args").read_text()
        self.assertIn("tcg,thread=multi", qemu)
        self.assertIn("hostfwd=tcp:127.0.0.1:23456-:22", qemu)
        for disk in ["silli-system", "silli-home", "silli-state"]:
            self.assertIn("serial=" + disk, qemu)
        self.assertIn("virtconsole,chardev=console", qemu)
        self.assertIn("virtio-net-pci,netdev=net,romfile=", qemu)
        ssh = (self.root / "ssh-args").read_text()
        self.assertIn("silli@127.0.0.1", ssh)
        self.assertIn("23456", ssh)

    def test_release_is_from_main_with_a_matching_lockfile(self):
        shutil.copy(ROOT / "packaging/check-release.py", self.root / "packaging/check-release.py")
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.0"\n')
        (self.root / "crates/demo").mkdir(parents=True)
        (self.root / "crates/demo/Cargo.toml").write_text('[package]\nname = "demo"\n')
        lock = self.root / "Cargo.lock"
        lock.write_text('[[package]]\nname = "demo"\nversion = "0.1.0"\n')
        changelog = self.root / "CHANGELOG.md"
        changelog.write_text("# Changelog\n\n## 0.1.0 — 2026-11-16\n\n- New.\n\n## 0.0.9 — 2026-11-15\n\n- Old.\n")
        def run(ref, *args):
            return subprocess.run(["python3", "packaging/check-release.py", *args], cwd=self.root,
                                  env=dict(self.env, GITHUB_REF=ref, GITHUB_OUTPUT=str(self.root / "output")),
                                  capture_output=True, text=True)
        def check(ref):
            return run(ref).returncode
        # the version's section is the release's notes; without one there is no release
        notes = run("refs/heads/main", "--notes")
        self.assertEqual((notes.returncode, notes.stdout), (0, "- New.\n"))
        changelog.write_text("# Changelog\n\n## 0.0.9 — 2026-11-15\n\n- Old.\n")
        self.assertNotEqual(check("refs/heads/main"), 0)
        self.assertNotEqual(run("refs/heads/main", "--notes").returncode, 0)
        self.assertFalse((self.root / "output").exists())
        changelog.write_text("## 0.1.0\n\n- New.\n")
        self.assertEqual(check("refs/heads/main"), 0)
        self.assertEqual((self.root / "output").read_text(), "version=0.1.0\n")
        self.assertNotEqual(check("refs/heads/other"), 0)
        self.assertNotEqual(check("refs/tags/v0.1.0"), 0)
        lock.write_text('[[package]]\nname = "demo"\nversion = "0.0.1"\n')
        self.assertNotEqual(check("refs/heads/main"), 0)


class InstallTests(unittest.TestCase):
    """site/install.sh against a pretend release: curl, sudo, apt-get and uname are stand-ins on PATH,
    the answers are typed on a pseudo-terminal because the script asks on /dev/tty."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "bin").mkdir()
        self.release = self.root / "release"
        self.release.mkdir()
        self.log = self.root / "log"
        self.log.write_text("")
        self.env = dict(os.environ, PATH=str(self.root / "bin") + ":" + os.environ["PATH"],
                        LOG=str(self.log), RELEASE=str(self.release), HOME=str(self.root))
        self.env.pop("SILLI_VERSION", None)
        self.env.pop("SILLI_DRY_RUN", None)
        self.system("Linux", "x86_64")
        self.latest("https://github.com/mattirk/silli/releases/tag/v0.1.0")
        self.command("sudo", 'echo "sudo $*" >> "$LOG"; exec "$@"')
        self.command("apt-get", 'echo "apt-get $*" >> "$LOG"')
        self.command("silli", 'echo "silli $*" >> "$LOG"')
        (self.release / "silli_0.1.0_amd64.deb").write_bytes(b"a package")
        script = b'#!/bin/sh\necho "unpacked silli $*" >> "$LOG"\n'
        with tarfile.open(self.release / "silli-0.1.0-x86_64.tar.gz", "w:gz") as tar:
            info = tarfile.TarInfo("silli-0.1.0-x86_64/silli")
            info.size, info.mode = len(script), 0o755
            tar.addfile(info, io.BytesIO(script))
        self.sums()

    def command(self, name, body):
        p = self.root / "bin" / name
        p.write_text("#!/bin/sh\nset -eu\n" + body + "\n")
        p.chmod(0o755)

    def system(self, kernel, machine):
        self.command("uname", f'case "$1" in -s) echo {kernel};; -m) echo {machine};; esac')

    def latest(self, url):
        # `curl … -I … URL/latest` answers where the redirect ends; `curl … -o FILE URL` copies a release file
        self.command("curl", f"""out=; head=; for a in "$@"; do
  if [ "$out" = next ]; then out=$a; fi
  case "$a" in -o) out=next;; -*I*) head=1;; esac; url=$a
done
if [ -n "$head" ]; then printf %s '{url}'; exit 0; fi
echo "curl $url" >> "$LOG"
cp "$RELEASE/${{url##*/}}" "$out" """)

    def sums(self):
        lines = [f"{hashlib.sha256(f.read_bytes()).hexdigest()}  {f.name}\n"
                 for f in sorted(self.release.iterdir()) if f.name != "SHA256SUMS"]
        (self.release / "SHA256SUMS").write_text("".join(lines))

    def install(self, answers, **env):
        """Runs the script with `answers` typed on its terminal: (exit status, what it printed)."""
        pid, fd = pty.fork()
        if pid == 0:
            os.execvpe("sh", ["sh", str(ROOT / "site/install.sh")], dict(self.env, **env))
        os.write(fd, answers.encode())
        out = b""
        while True:
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
        os.close(fd)
        return os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]), out.decode(errors="replace")

    def test_debian_package_is_verified_and_installed(self):
        status, out = self.install("1\ny\n")
        self.assertEqual(status, 0, out)
        self.assertIn("checksum ok", out)
        log = self.log.read_text()
        self.assertIn("releases/download/v0.1.0/silli_0.1.0_amd64.deb", log)
        self.assertRegex(log, r"sudo apt-get install -y \S+/silli_0\.1\.0_amd64\.deb")
        self.assertIn("silli init --user", log)

    def test_tarball_runs_its_own_init_and_user_start_can_be_declined(self):
        status, out = self.install("2\nn\n")
        self.assertEqual(status, 0, out)
        log = self.log.read_text()
        self.assertIn("unpacked silli init", log)
        self.assertNotIn("silli init --user", log)

    def test_wrong_checksum_installs_nothing(self):
        (self.release / "silli_0.1.0_amd64.deb").write_bytes(b"another package")
        status, out = self.install("1\ny\n")
        self.assertNotEqual(status, 0)
        self.assertIn("checksum mismatch", out)
        self.assertNotIn("sudo", self.log.read_text())

    def test_cancel_plan_and_unknown_answers_change_nothing(self):
        for answers in ["4\n", "\n", "nonsense\n", "9\n", "3\ny\n"]:
            status, out = self.install(answers)
            self.assertEqual(status, 0, out)
            self.assertEqual(self.log.read_text(), "", answers)
        self.assertIn("would run: sudo", out)

    def test_without_a_terminal_it_stops_before_anything(self):
        r = subprocess.run(["sh", str(ROOT / "site/install.sh")], env=self.env, stdin=subprocess.DEVNULL,
                           capture_output=True, text=True, start_new_session=True)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("no terminal", r.stderr)
        self.assertEqual(self.log.read_text(), "")

    def test_mac_plan(self):
        self.system("Darwin", "arm64")
        status, out = self.install("1\n", SILLI_DRY_RUN="1")
        self.assertEqual(status, 0, out)
        self.assertIn("releases/download/v0.1.0/silli-0.1.0-macos-arm64.dmg", out)
        self.assertIn("would run: cp -R", out)
        self.assertLess(out.index("Silli.app.new"), out.index("rm -rf /Applications/Silli.app\r"), "the copy comes before the removal")
        self.assertEqual(self.log.read_text(), "")
        self.system("Darwin", "x86_64")
        status, out = self.install("1\n", SILLI_DRY_RUN="1")
        self.assertNotEqual(status, 0)
        self.assertIn("Apple Silicon", out)

    def test_mac_removes_its_own_old_app_and_no_other_sill(self):
        self.system("Darwin", "arm64")
        self.command("defaults", "echo com.github.mattirk.sill")
        status, out = self.install("1\n", SILLI_DRY_RUN="1")
        self.assertEqual(status, 0, out)
        self.assertIn("would run: rm -rf /Applications/Sill.app\r", out)
        self.command("defaults", "echo com.example.sill")
        status, out = self.install("1\n", SILLI_DRY_RUN="1")
        self.assertEqual(status, 0, out)
        self.assertNotIn("/Applications/Sill.app", out)

    def test_systems_without_a_package_and_a_missing_release_are_named(self):
        self.system("Linux", "aarch64")
        status, out = self.install("1\n")
        self.assertNotEqual(status, 0)
        self.assertIn("no Linux package for aarch64", out)
        self.system("Linux", "x86_64")
        self.latest("https://github.com/mattirk/silli/releases")
        status, out = self.install("1\n")
        self.assertNotEqual(status, 0)
        self.assertIn("no published release", out)

    def test_a_version_is_only_version_characters(self):
        for version in ["../../x", "0.1.0/..", "0.1.0 x", "$(id)"]:
            status, out = self.install("1\n", SILLI_VERSION=version)
            self.assertNotEqual(status, 0, version)
            self.assertIn("not a version", out)
            self.assertEqual(self.log.read_text(), "")

    def test_page_offers_the_script_it_ships_and_the_names_the_release_builds(self):
        page = (ROOT / "site/index.html").read_text()
        self.assertIn("curl -fsSL https://mattirk.github.io/silli/install.sh | sh", page)
        self.assertIn('href="install.sh"', page)
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("macos-arm64.dmg", workflow)
        self.assertIn("macos-arm64", page)


if __name__ == "__main__":
    unittest.main()
