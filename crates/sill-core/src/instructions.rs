//! Standing instructions (SPEC §3.1): what every agent is told once per agent start, whatever its
//! type — sill's own facts about the jail, then the person's `~/.config/sill/instructions.md`. The
//! block goes with the first prompt after the agent (re)starts and again with the next prompt after
//! it changed; pins stay a per-prompt block of their own. Layers append, never replace: agent-type
//! and per-session instructions come after the global file when they arrive.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// What sill tells every agent: facts about the jail that change with sill's releases, not with the
/// person — kept here rather than in the person's file so they cannot go stale.
pub const BASE: &str = "[A command that must keep running after your turn (a dev server, a watcher): `sill-proc start --name NAME [--port PORT] -- COMMAND…`; `sill-proc list|tail NAME|stop NAME` (`/.sill/bin/sill-proc` when it is not on PATH). A background `&` dies with you.]";

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

/// The block sent to the agent: [`BASE`], then the person's instructions when there are any — each
/// layer a bracketed block on lines of its own, like the pin context.
pub fn standing_block(global: &str) -> String {
    let mut out = format!("{BASE}\n");
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
            assert_eq!(standing_block(g), format!("{BASE}\n"));
        }
    }

    #[test]
    fn the_persons_text_follows_the_base_trimmed() {
        let b = standing_block("\n\nAnswer in Finnish.\nRun `cargo test` before you say done.\n\n");
        assert!(b.starts_with(&format!("{BASE}\n[Standing instructions from the person")));
        assert!(b.ends_with(":\nAnswer in Finnish.\nRun `cargo test` before you say done.]\n"));
    }
}
