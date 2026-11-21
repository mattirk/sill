# Builds and releases

The public repository runs `build` for pushes to `main`, pull requests targeting
`main`, and manual dispatch. It tests and compiles Linux x86_64 and macOS ARM64.
Download the binary archives from the workflow's artifacts. The Mac build archive
is a developer binary, not a complete app: it does not include the Linux VM image.

Ubuntu 24.04 denies unprivileged user namespaces through AppArmor, so both workflows
switch that off on the runner first
(`sysctl kernel.apparmor_restrict_unprivileged_userns=0`) and check that `bwrap`
starts. The daemon's login tests (`crates/silli/tests/daemon.rs`) do not need it:
they put a stand-in for `bwrap` on the daemon's PATH. Tests of the real jail are
ignored by default (`requires working bubblewrap on a host`).

## Release a version

1. Update `[workspace.package].version` in Cargo.toml and regenerate Cargo.lock
   (`cargo check -p silli-proto` also refreshes workspace package versions).
2. Add a `## VERSION — DATE` section to CHANGELOG.md with what changed since the last
   release. It becomes the release's notes on GitHub; a version without one is not released
   (`packaging/check-release.py`; `--notes` prints the section).
3. Commit and push private master, then run its publication workflow
   (`gh workflow run publish`).

That is all. Every push to public `main` starts the `release` workflow. If the
workspace version has no `vVERSION` tag yet, it runs the tests,
builds the Linux tar/deb packages and the Apple Silicon DMG, and publishes the
release with its CHANGELOG.md section as notes and one SHA256SUMS; publishing creates the tag at the
built commit. It then runs `site/install.sh` against the new release on a Linux and
a Mac runner. A version that is already tagged does nothing, so only a version bump
releases. Nothing is published unless every package was built; after a failed run,
the next push to `main` tries again.

Not covered by CI: the Linux VM image is never booted under Apple's Virtualization
framework, and the unsigned-app prompt is not exercised. Open a released DMG on an
Apple Silicon Mac when the image or the app packaging changed. A bad release is
withdrawn by deleting it and its tag on GitHub; the installer then offers the
previous one.

## Download page and installer

`site/` is the download page, served by GitHub Pages at <https://mattirk.github.io/silli/>.
The public `pages` workflow deploys it on pushes to `main` that change `site/`, and on
manual dispatch. It needs Settings → Pages → Source set to **GitHub Actions** once.

`site/install.sh` is what the page tells people to paste (`curl … | sh`). It follows
`releases/latest` to the newest **published** release, so a draft is never offered, and it
expects the package names the release workflow produces (`silli-VERSION-x86_64.tar.gz`,
`silli_VERSION_amd64.deb`, `silli-VERSION-macos-arm64.dmg`, `SHA256SUMS`). Rename them in both
places. `python3 packaging/test_release.py InstallTests` runs it against a pretend release.
The `release` workflow runs it for real against each release it publishes
(`packaging/try-install.py`).

## Mac image and packaging

The local Apple Silicon build remains:

```sh
packaging/mac-image.sh
packaging/mac/app.sh
```

This still boots Apple's Virtualization framework and compiles inside the Debian
ARM64 guest. No Linux runner or Docker is required for this path. Local app builds
may reuse their existing VM image when only the app changes.

CI builds the same guest image on an `ubuntu-24.04-arm` runner:

```sh
export SILLI_IMAGE_CACHE="$PWD/target/image-cache"
export SILLI_IMAGE_BINARIES="$PWD/target/mac-prebuilt"
packaging/mac/prebuild-linux.sh
packaging/mac-image.sh
```

The Linux host needs Docker, QEMU (`qemu-system-arm`), AAVMF firmware
(`qemu-efi-aarch64`), `cloud-image-utils`, Perl's Digest::SHA, SSH and Rust.
Native Debian 13 ARM64 containers compile the daemon, shim and Weston. QEMU TCG
then boots the guest to run the shared setup, deployment and cleanup. This needs
no nested hardware virtualization. Build SSH is forwarded only on localhost;
its key and sudo access are removed from the delivered image.

CI transfers the compressed raw image, its original size and commit identity,
plus checksums, to a `macos-15` ARM64 runner. `app.sh` accepts this compressed-only
input as well as the local raw image. CI requires the image to match the released
commit with `SILLI_IMAGE_EXPECT_COMMIT`; it verifies transfer checksums before
packaging. No VM is booted on GitHub's hosted Mac.

CI uses free **ad-hoc signing**, with the existing virtualization entitlement.
There is no Developer ID certificate or Apple notarization. Local builds still
prefer the `Silli local development` certificate when installed. An explicit
`SILLI_SIGNING_IDENTITY` overrides that choice (`-` means ad-hoc). Official signing
would additionally require a CI keychain import and notarization/stapling steps;
these are not configured yet.

For downloaded Mac releases, drag Silli into Applications and try opening it.
macOS refuses the first open of a browser download, on every Mac, because the app
is not notarized: use System Settings → Privacy & Security → Open Anyway for Silli,
once per installed version. Confirmed with the 0.0.3 release from GitHub
(2026-11-15): it opens and runs after that. Organization-managed Macs may prevent
exceptions.

Hosted CI cannot exercise Apple's VM framework, so the Linux/QEMU-generated image
is not boot-tested there.
