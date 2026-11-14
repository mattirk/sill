//! Explicit render-node access. No display-control or proprietary NVIDIA devices.
use std::os::unix::fs::FileTypeExt;
use std::{fs, path::PathBuf};

#[derive(Debug)]
pub struct Probe {
    pub node: Option<PathBuf>,
    pub missing: String,
}
impl Probe {
    pub fn available(&self) -> bool {
        self.node.is_some()
    }
}

/// Reprobe at launch so a removed device never silently falls back to software.
pub fn probe() -> Probe {
    let mut nodes: Vec<_> = fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    nodes.sort();
    for node in nodes {
        let Some(name) = node.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !render_name(name) {
            continue;
        }
        if !fs::symlink_metadata(&node).is_ok_and(|m| m.file_type().is_char_device()) {
            continue;
        }
        let driver = fs::canonicalize(format!("/sys/class/drm/{name}/device/driver")).ok();
        // Restrict this first backend to the Mesa Intel/AMD drivers.
        if !driver
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .is_some_and(|n| matches!(n, "i915" | "xe" | "amdgpu" | "radeon"))
        {
            continue;
        }
        if fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .is_ok()
        {
            return Probe {
                node: Some(node),
                missing: String::new(),
            };
        }
    }
    Probe {
        node: None,
        missing:
            "no accessible Intel/AMD GPU render node (NVIDIA is not supported by this backend)"
                .into(),
    }
}

fn render_name(name: &str) -> bool {
    name.strip_prefix("renderD")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_render_node_names() {
        assert!(render_name("renderD128"));
        for name in [
            "card0",
            "renderD",
            "renderD128/../card0",
            "controlD64",
            "nvidia0",
        ] {
            assert!(!render_name(name));
        }
    }
}
