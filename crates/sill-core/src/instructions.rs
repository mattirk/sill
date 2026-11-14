//! Standing instructions (SPEC §3.1): what every agent is told once per agent start, whatever its
//! type — sill's own facts about the jail (fixed ones in [`BASE`], the session's policy and the
//! host's tool directories in [`facts_block`]), then the person's `~/.config/sill/instructions.md`. The
//! block goes with the first prompt after the agent (re)starts and again with the next prompt after
//! it changed; pins stay a per-prompt block of their own. Layers append, never replace: agent-type
//! and per-session instructions come after the global file when they arrive.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::policy::{NetMode, Policy};

/// What sill tells every agent: facts about the jail that change with sill's releases, not with the
/// person — kept here rather than in the person's file so they cannot go stale.
pub const BASE: &str = "[A command that must keep running after your turn (a dev server, a watcher): `sill-proc start --name NAME [--port PORT] -- COMMAND…`; `sill-proc list|tail NAME|stop NAME` (`/.sill/bin/sill-proc` when it is not on PATH). A background `&` dies with you.]";

/// Directories with tools the person installed under their home that the jail's PATH
/// (`/.sill/bin`, `~/.local/bin`, `~/.cargo/bin`, the system dirs) does not have: an agent otherwise
/// spends a few tool calls finding `cargo` or `go`. Only what exists, as `~/…`; the rustup
/// toolchain dir only when `~/.cargo/bin` (rustup's usual proxies) is not there.
pub fn tool_dirs(home: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |p: PathBuf| {
        if p.is_dir()
            && let Ok(rel) = p.strip_prefix(home)
        {
            out.push(format!("~/{}", rel.display()));
        }
    };
    if !home.join(".cargo/bin").is_dir()
        && let Ok(rd) = std::fs::read_dir(home.join(".rustup/toolchains"))
    {
        let mut tc: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path().join("bin")).filter(|p| p.join("cargo").is_file()).collect();
        tc.sort();
        if let Some(stable) = tc.iter().find(|p| p.to_string_lossy().contains("/stable-")).cloned().or_else(|| tc.first().cloned()) {
            add(stable);
        }
    }
    for rel in ["go/bin", ".npm-global/bin", ".bun/bin", ".deno/bin", ".volta/bin", ".pyenv/bin", ".local/share/pnpm", ".nix-profile/bin", ".ghcup/bin", ".dotnet/tools"] {
        add(home.join(rel));
    }
    if let Ok(rd) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut v: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path().join("bin")).collect();
        v.sort();
        if let Some(latest) = v.pop() {
            add(latest);
        }
    }
    out
}

/// `api.anthropic.com`, `*.anthropic.com` and `anthropic.com` count once, as `anthropic.com`: the
/// last two labels (a sketch of the list, not the rules — the proxy has those).
fn apexes(patterns: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in patterns {
        let host = p.trim_start_matches("*.").split(':').next().unwrap_or("");
        let labels: Vec<&str> = host.split('.').collect();
        let apex = labels[labels.len().saturating_sub(2)..].join(".");
        if !apex.is_empty() && !out.contains(&apex) {
            out.push(apex);
        }
    }
    out
}

/// A list for a sentence: the first `n`, then "and N more".
fn some(items: &[String], n: usize) -> String {
    if items.len() <= n {
        items.join(", ")
    } else {
        format!("{} and {} more", items[..n].join(", "), items.len() - n)
    }
}

/// What this session's jail is like, from its policy and the host (SPEC §3.1): the write model,
/// scratch and hidden paths, desktop access, network, tools off the PATH, and that the host's own
/// changes meanwhile may not show. One bracketed block, a few lines — every token here is paid
/// once per agent start, but saved on every prompt that would otherwise go looking.
pub fn facts_block(policy: &Policy, home: &Path) -> String {
    let p = &policy.paths;
    let mut lines: Vec<String> = Vec::new();
    lines.push("You run in sill's jail. `/` is a view of the host: a write outside a pinned folder goes to this session's overlay and reaches the host only when the person keeps it (Changes); a pinned folder is the host itself — a write or a commit there is a host write at once. `sudo` and setuid do not work; system changes are for the person to keep.".into());
    if !p.private.is_empty() {
        lines.push(format!("Kept across turns, never on the host: {}.", p.private.join(", ")));
    }
    if !p.direct.is_empty() {
        lines.push(format!("Written straight to the host, unrecorded: {}.", p.direct.join(", ")));
    }
    if !p.mask.is_empty() {
        lines.push(format!("Hidden (empty here): {}.", some(&p.mask, 10)));
    }
    if !p.ro.is_empty() {
        lines.push(format!("Read-only: {}.", some(&p.ro, 10)));
    }
    let desktop = match (policy.jail.x11, policy.jail.dbus) {
        (true, true) => "X11 and the D-Bus session bus are available.",
        (true, false) => "X11 is available (GUI programs run and show); no D-Bus session bus.",
        (false, true) => "The D-Bus session bus is available; no X11.",
        (false, false) => "No X11, no D-Bus: nothing graphical, no desktop services.",
    };
    lines.push(desktop.into());
    let net = match policy.network.mode {
        NetMode::None => "No network.".to_string(),
        NetMode::All => "Network: any host, through sill's proxy (host-local addresses refused unless the policy lists them).".to_string(),
        NetMode::Allow => format!(
            "Network: an allow-list through sill's proxy ({}); a refused host is shown to the person with an Allow button, so name what you needed and move on instead of retrying.",
            some(&apexes(&policy.network.allow), 8)
        ),
    };
    lines.push(net);
    let tools = tool_dirs(home);
    if !tools.is_empty() {
        lines.push(format!("Not on PATH but present: {}.", tools.join(", ")));
    }
    lines.push("What changes on the host outside pinned folders while this session runs (an install, a build done on the host) may not show here until the session restarts.".into());
    format!("[{}]", lines.join("
"))
}

/// The person's global instructions: Markdown, any agent. Under `~/.config/sill`, which the jail
/// masks — the agent neither reads nor edits its own instructions.
pub fn global_path() -> PathBuf {
    crate::config::dir().join("instructions.md")
}

/// The person's global instructions; no file = none.
pub fn load_global() -> String {
    std::fs::read_to_string(global_path()).unwrap_or_default()
}

/// Save the person's global instructions; blank removes the file.
pub fn save_global(text: &str) -> Result<PathBuf> {
    let p = global_path();
    if text.trim().is_empty() {
        return match std::fs::remove_file(&p) {
            Ok(()) => Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(p),
            Err(e) => Err(e).with_context(|| format!("remove {}", p.display())),
        };
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, text).with_context(|| format!("write {}", p.display()))?;
    Ok(p)
}

/// The block sent to the agent: [`BASE`], the session's [`facts_block`] (empty when the policy
/// could not be read), then the person's instructions when there are any — each layer a bracketed
/// block on lines of its own, like the pin context.
pub fn standing_block(facts: &str, global: &str) -> String {
    let mut out = format!("{BASE}\n");
    if !facts.is_empty() {
        out.push_str(facts);
        out.push('\n');
    }
    let g = global.trim();
    if !g.is_empty() {
        out.push_str(&format!("[Standing instructions from the person (~/.config/sill/instructions.md) — they hold until changed:\n{g}]\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_alone_when_the_person_wrote_nothing() {
        for g in ["", "  \n\t"] {
            assert_eq!(standing_block("", g), format!("{BASE}\n"));
        }
    }

    #[test]
    fn the_persons_text_follows_the_base_trimmed() {
        let b = standing_block("[facts]", "\n\nAnswer in Finnish.\nRun `cargo test` before you say done.\n\n");
        assert!(b.starts_with(&format!("{BASE}\n[facts]\n[Standing instructions from the person")));
        assert!(b.ends_with(":\nAnswer in Finnish.\nRun `cargo test` before you say done.]\n"));
    }

    /// The facts follow the policy: what it masks, keeps private, allows; the tool dirs the host has.
    #[test]
    fn facts_follow_the_policy_and_the_host() {
        let home = std::env::temp_dir().join(format!("sill-facts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin")).unwrap();
        std::fs::write(home.join(".rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo"), "").unwrap();
        std::fs::create_dir_all(home.join("go/bin")).unwrap();
        let p: Policy = toml::from_str(
            r#"
name = "t"
[network]
mode = "allow"
allow = ["api.anthropic.com", "*.anthropic.com", "github.com", "*.github.com", "crates.io"]
[paths]
private = ["~/.cache"]
mask = ["~/.ssh", "~/.gnupg"]
ro = ["/srv/music"]
[jail]
x11 = true
"#,
        )
        .unwrap();
        let f = facts_block(&p, &home);
        assert!(f.starts_with("[You run in sill's jail."), "{f}");
        assert!(f.ends_with("until the session restarts.]"), "{f}");
        assert!(f.contains("never on the host: ~/.cache."));
        assert!(f.contains("Hidden (empty here): ~/.ssh, ~/.gnupg."));
        assert!(f.contains("Read-only: /srv/music."));
        assert!(f.contains("X11 is available"));
        assert!(f.contains("allow-list through sill's proxy (anthropic.com, github.com, crates.io)"), "{f}");
        assert!(f.contains("Not on PATH but present: ~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin, ~/go/bin."), "{f}");
        // rustup's proxies on the PATH: the toolchain dir is not named
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        assert_eq!(tool_dirs(&home), vec!["~/go/bin".to_string()]);
        let none: Policy = toml::from_str("name = \"n\"\n[network]\nmode = \"none\"").unwrap();
        let f = facts_block(&none, &home);
        assert!(f.contains("No network.") && f.contains("No X11, no D-Bus") && !f.contains("Hidden"));
        // the long lists are cut
        let many: Vec<String> = (0..12).map(|i| format!("~/m{i}")).collect();
        assert_eq!(some(&many, 10), format!("{} and 2 more", many[..10].join(", ")));
        std::fs::remove_dir_all(&home).unwrap();
    }
}
