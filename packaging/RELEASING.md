# Builds and releases

The public repository runs `build` for pushes to `main`, pull requests targeting
`main`, and manual dispatch. It tests and compiles Linux x86_64 and macOS ARM64.
Download the binary archives from the workflow's artifacts. The Mac build archive
is a developer binary, not a complete app: it does not include the Linux VM image.

## Release a version

1. Update `[workspace.package].version` in Cargo.toml and regenerate Cargo.lock
   (`cargo check -p sill-proto` also refreshes workspace package versions).
2. Commit and push private master, then manually run its publication workflow.
3. Wait for the public build checks and test the artifacts.
4. Push a `vVERSION` tag to the verified commit in the **public** repository.
   For example, `v0.1.0` requires workspace and lockfile versions of `0.1.0`.
5. The public `release` workflow builds Linux tar/deb packages and the Apple
   Silicon DMG. It creates a **draft** release only after both platforms succeed,
   with generated notes and one SHA256SUMS covering every package.
6. Test the downloaded packages, edit the draft's notes, then publish it.

The workflow can also be dispatched against an existing version tag. Branch refs
and mismatched versions are rejected. Release jobs do not run in the private repo.
Tagging does not publish a release immediately: the final publish is a human step.

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
export SILL_IMAGE_CACHE="$PWD/target/image-cache"
export SILL_IMAGE_BINARIES="$PWD/target/mac-prebuilt"
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
input as well as the local raw image. CI requires the image to match the tagged
commit with `SILL_IMAGE_EXPECT_COMMIT`; it verifies transfer checksums before
packaging. No VM is booted on GitHub's hosted Mac.

CI uses free **ad-hoc signing**, with the existing virtualization entitlement.
There is no Developer ID certificate or Apple notarization. Local builds still
prefer the `Sill local development` certificate when installed. An explicit
`SILL_SIGNING_IDENTITY` overrides that choice (`-` means ad-hoc). Official signing
would additionally require a CI keychain import and notarization/stapling steps;
these are not configured yet.

For downloaded Mac releases, drag Sill into Applications and try opening it.
If macOS blocks the unidentified developer, use System Settings → Privacy &
Security → Open Anyway for Sill. Test this download/open flow on a separate Mac
before publishing; organization-managed Macs may prevent exceptions.

The Linux/QEMU-generated image must also be boot-tested on a real Apple Silicon
Mac before publishing the first release. Hosted CI cannot exercise Apple's VM
framework. The workflow's tests and assembly checks do not replace that test.
