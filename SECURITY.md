# Security

Sill runs AI coding agents in a jail and lets you decide what they may change, reach and keep.
It reduces what an agent can do to your machine; it does not guarantee security. This page says
how to report a problem, what sill is built to hold against, and where it is known not to.

## Reporting a vulnerability

Open a public [issue](https://github.com/mattirk/sill/issues), or a pull request with a fix.

If you would rather report privately, use GitHub's
**[Report a vulnerability](https://github.com/mattirk/sill/security/advisories/new)** (the
repository's Security tab → Advisories). A private report is visible only to the author.

Useful in a report: the sill version or commit, the system (Linux distribution and kernel, or
macOS version), the policy in use if it is not the default, and the steps or a proof of concept.

Sill is a side project of one author. A report may or may not get an answer or a fix. Reports
about agents themselves (Claude Code, Codex and others) belong to their vendors, unless the
problem is how sill runs them.

## Supported versions

Sill is pre-1.0 and comes without support. If a problem is fixed, the fix goes to the `main`
branch and into a later release; earlier releases are not updated.

## What sill is built to hold against

The agent inside a session is assumed to be fully hostile, for example through a prompt injection:
it controls every file it writes, every command it runs, the text of its tool calls and the
network requests it makes. Against that agent, sill aims to keep:

- **The host's files.** The agent writes to an overlay; changes reach the host only when you keep
  them. Folders you pin are the exception: they are written directly.
- **Your secrets.** The paths the policy masks (`~/.ssh`, `~/.gnupg`, `~/.config`, cloud
  credentials and others) are empty inside the jail.
- **The network.** The jail has no network of its own. Requests go through sill's proxy, which
  allows only the hosts on the policy's list and refuses local and private addresses.
- **Other sessions and sill itself.** No capabilities, no nested user namespaces, no host D-Bus or
  display, and sill's own configuration and state are hidden.

Sill also considers a second unprivileged account on the same machine, a hostile remote box, and
installing sill from a checkout an agent has modified.

## Known limits

- **The jail shares the host's kernel.** A kernel vulnerability reachable from the jail is a way
  out. On x86_64 a syscall filter narrows that surface; **on arm64, including the Linux VM inside
  Sill.app, there is no syscall filter.**
- **The Mac VM is not a second wall of equal strength.** It keeps the agent off macOS itself, but
  the folders you share with it are writable from the whole VM, and sessions inside one VM are
  separated less strictly than on a Linux desktop.
- **Pinned folders are live.** An agent can write anything in a pin, including files that run
  later on the host: build scripts, git hooks, editor tasks. Pin what you would let the agent edit.
- **Keep is your review.** Sill shows what changed and refuses some dangerous paths, but what you
  keep is on the host. Read what "Keep all" covers before using it.
- **The default mask list is not complete.** Credentials stored outside the masked paths (for
  example `~/.pgpass`, `~/.terraform.d`, or Snap and Flatpak browser profiles) are readable unless
  you add them to your policy.
- **Allowed hosts are a way out for data.** If the policy allows `github.com`, an agent holding a
  token can send data there. The allow-list limits where data can go, not what is sent.
- **Grants are wide.** Host control (`launch`, `commands`), "Control Mac" and "Run Mac commands"
  each amount to acting as you on that machine. Treat any of them as full access for as long as
  it is on; revoking stops what sill still tracks, not what was installed or detached meanwhile.
- **Installing an agent means trusting its publisher.** Sill pins and checks the packages in its
  own registry snapshot and probes a new agent inside a jail, but logging an agent in and running
  it gives its code your account at that provider, and some login flows run on the host.
- **Releases are not signed.** Packages come with a `SHA256SUMS` file from the same release, which
  detects a damaged download but not a replaced one. The Mac app is ad-hoc signed and not
  notarized.
- **No independent audit.** Sill is developed with AI coding agents and reviewed by its author.
  The audits below were done the same way.

## Audits

- **2026-11-01** — [docs/security.md](docs/security.md): the jail, the proxy, Keep and the root
  install; findings S1–S21 with their status.
- **2026-11-15** — a second audit of the same areas plus the Mac app, the daemon protocol and the
  release pipeline. It confirmed that most earlier fixes hold and found new issues, some of them
  serious and open. The report is not published while they are open.

## Checking a download

```bash
sha256sum -c SHA256SUMS --ignore-missing
```

This confirms the file arrived intact. It does not prove who built it.
