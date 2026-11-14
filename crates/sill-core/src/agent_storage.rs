//! Explicit agent storage and a read-only filesystem plan. Contents never confer ownership.
use crate::agents::{Agent, HomePath, HomePathKind};
use crate::paths::{expand_home, expand_pattern, layer_name};
use crate::policy::Policy;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sill_proto::admin::{FilesystemView, PathRule};
use sill_proto::sessions::HostPath;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub source: HostPath,
    pub target: HostPath,
    pub declaration: Option<HomePath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMask {
    pub path: HostPath,
    pub kind: HomePathKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyPaths {
    pub masks: Vec<AgentMask>,
    pub direct: Vec<HostPath>,
    pub read_only: Vec<HostPath>,
}

/// A workspace session's repository (docs/parallel-coding.md §12): its clone is bound at the
/// repository's own path, the person's checkout hidden from the read-only host view too, and the
/// checkout's object store borrowed read-only at a private path the clone's alternates name there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBind {
    pub source: HostPath,
    pub clone: HostPath,
    pub objects: HostPath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub profile: HostPath,
    pub ghost: bool,
    pub normal_profile: HostPath,
    pub credentials: crate::credentials::Rules,
    pub bindings: Vec<Binding>,
    pub agent_masks: Vec<AgentMask>,
    pub policy_paths: PolicyPaths,
    pub view: FilesystemView,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceBind>,
}

/// No storage preparation, credential import or filesystem writes occur during resolution.
pub fn resolve(
    agent: &Agent,
    registry: &BTreeMap<String, Agent>,
    policy: &Policy,
    state_dir: &Path,
    home: &Path,
    pins: &[PathBuf],
    ghost: bool,
) -> Result<Plan> {
    let roots = agent.ready(home)?;
    let profile = if ghost {
        crate::session::ghost_agent_home(state_dir, &agent.id)
    } else {
        crate::session::agent_home(home, &agent.id)
    };
    let mut plan = Plan {
        profile: HostPath::from_path(&profile),
        ghost,
        normal_profile: HostPath::from_path(&crate::session::agent_home(home, &agent.id)),
        credentials: crate::credentials::Rules::for_agent(agent)?,
        bindings: vec![],
        agent_masks: vec![],
        policy_paths: PolicyPaths::default(),
        view: FilesystemView {
            rules: vec![],
            unused: vec![],
        },
        workspaces: vec![],
    };
    plan.rule(
        home,
        Some(home),
        "overlay",
        "policy",
        "Host files are readable; writes stay in this session except for explicit bindings.",
    );
    for entry in roots {
        let relative =
            crate::agents::relative_path(entry.path.strip_prefix("~/").context("home path")?)?;
        let target = home.join(&relative);
        let source = profile.join(relative);
        plan.bindings.push(Binding {
            source: HostPath::from_path(&source),
            target: HostPath::from_path(&target),
            declaration: Some(entry.clone()),
        });
        plan.rule(
            &target,
            Some(&source),
            "agent",
            &format!("agent {}: home_paths", agent.id),
            if ghost {
                "Session-private agent state; only declared credentials are imported."
            } else {
                "Persistent private profile shared by this agent's normal sessions."
            },
        );
    }
    for credential in &plan.credentials.files.clone() {
        plan.rule(&home.join(credential), Some(&profile.join(credential)), "credential", "agent: login_files", "Only this declared file may be imported or synchronized between the normal profile and ghosts.");
    }
    for other in registry.values() {
        other.validate()?;
        for entry in other.home_paths.as_deref().unwrap_or_default() {
            let path = expand_home(&entry.path, home);
            if let Some(mask) = plan
                .agent_masks
                .iter()
                .find(|p| p.path == HostPath::from_path(&path))
            {
                ensure!(
                    mask.kind == entry.kind,
                    "agent definitions disagree about the type of {}",
                    path.display()
                );
            } else {
                plan.agent_masks.push(AgentMask {
                    path: HostPath::from_path(&path),
                    kind: entry.kind,
                });
                plan.rule(&path, None, "agent_mask", &format!("agent {}: home_paths", other.id), "Original host agent data is hidden in both views; selected private profile bindings are separate.");
            }
        }
    }
    for pin in pins.iter().filter(|_| !ghost) {
        if pin.is_dir() {
            plan.bindings.push(Binding {
                source: HostPath::from_path(pin),
                target: HostPath::from_path(pin),
                declaration: None,
            });
            plan.rule(
                pin,
                Some(pin),
                "pin",
                "session pins",
                "Writes reach the pinned host folder directly.",
            );
        }
    }
    for pattern in &policy.paths.private {
        for target in expanded(pattern, home) {
            let source = state_dir.join("private").join(layer_name(&target));
            plan.bindings.push(Binding {
                source: HostPath::from_path(&source),
                target: HostPath::from_path(&target),
                declaration: None,
            });
            plan.rule(
                &target,
                Some(&source),
                "private",
                "policy: paths.private",
                "Session-private scratch; no host cache import.",
            );
        }
    }
    for (patterns, behavior, reason) in [
        (
            &policy.paths.mask,
            "mask",
            "Hidden by the policy in both views.",
        ),
        (&policy.paths.ro, "read_only", "Read-only host binding."),
        (
            &policy.paths.direct,
            "direct",
            "Writes reach the host directly.",
        ),
    ] {
        for pattern in patterns {
            for target in expand_pattern(pattern, home) {
                plan.rule(
                    &target,
                    None,
                    behavior,
                    &format!(
                        "policy: paths.{}",
                        if behavior == "read_only" {
                            "ro"
                        } else {
                            behavior
                        }
                    ),
                    reason,
                );
            }
        }
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    for target in [
        config.join("sill"),
        state.join("sill"),
        PathBuf::from("/var/log/journal"),
    ] {
        plan.rule(
            &target,
            None,
            "mask",
            "Sill internal storage",
            "Sill configuration, profiles and daemon records stay hidden.",
        );
    }
    for rule in &plan.view.rules {
        let target = rule.target.to_path()?;
        match rule.behavior.as_str() {
            "mask" => plan.policy_paths.masks.push(AgentMask {
                path: rule.target.clone(),
                kind: if target.is_file() {
                    HomePathKind::File
                } else {
                    HomePathKind::Directory
                },
            }),
            "read_only" => plan.policy_paths.read_only.push(rule.target.clone()),
            "direct" => plan.policy_paths.direct.push(rule.target.clone()),
            _ => (),
        }
    }
    // A private root must never compete with a pin, private scratch, or another declared root.
    for (i, binding) in plan.bindings.iter().enumerate() {
        let target = binding.target.to_path()?;
        for other in &plan.bindings[..i] {
            let parent = other.target.to_path()?;
            ensure!(
                !(binding.declaration.is_some() || other.declaration.is_some())
                    || !(target.starts_with(&parent) || parent.starts_with(&target)),
                "agent storage conflicts with another binding: {} and {}",
                target.display(),
                parent.display()
            );
        }
        if binding.declaration.is_some() {
            for pattern in policy
                .paths
                .mask
                .iter()
                .chain(&policy.paths.ro)
                .chain(&policy.paths.direct)
            {
                for restricted in expanded(pattern, home) {
                    ensure!(
                        !(target.starts_with(&restricted) || restricted.starts_with(&target)),
                        "agent storage {} conflicts with policy path {}",
                        target.display(),
                        restricted.display()
                    );
                }
            }
            for rule in plan
                .view
                .rules
                .iter()
                .filter(|r| matches!(r.behavior.as_str(), "mask" | "direct" | "read_only"))
            {
                let restricted = rule.target.to_path()?;
                ensure!(
                    !(target.starts_with(&restricted) || restricted.starts_with(&target)),
                    "agent storage {} conflicts with {} at {}",
                    target.display(),
                    rule.behavior,
                    restricted.display()
                );
            }
        }
    }
    plan.view.unused = unused(&crate::session::agent_home(home, &agent.id), roots)?;
    Ok(plan)
}

/// A development terminal has no persistent agent state; its explicit binds are still recorded.
pub fn resolve_development(
    registry: &BTreeMap<String, Agent>,
    policy: &Policy,
    state_dir: &Path,
    home: &Path,
    binds: &[(PathBuf, PathBuf)],
) -> Result<Plan> {
    let stateless: Agent = toml::from_str(
        "id = 'sill-development'\nname = 'Development terminal'\ncommand = []\nhome_paths = []",
    )?;
    let mut plan = resolve(&stateless, registry, policy, state_dir, home, &[], false)?;
    for (source, target) in binds {
        ensure!(
            !plan.bindings.iter().any(|b| b
                .target
                .to_path()
                .is_ok_and(|p| p.starts_with(target) || target.starts_with(p))),
            "development binding conflicts with private scratch"
        );
        plan.bindings.push(Binding {
            source: HostPath::from_path(source),
            target: HostPath::from_path(target),
            declaration: None,
        });
        plan.rule(
            target,
            Some(source),
            "direct",
            "development --bind",
            "Explicit development-jail binding.",
        );
    }
    Ok(plan)
}

fn expanded(pattern: &str, home: &Path) -> Vec<PathBuf> {
    if pattern.contains(['*', '?', '[']) {
        expand_pattern(pattern, home)
    } else {
        vec![expand_home(pattern, home)]
    }
}

fn unused(base: &Path, roots: &[HomePath]) -> Result<Vec<HostPath>> {
    let declared: Vec<PathBuf> = roots
        .iter()
        .map(|r| PathBuf::from(r.path.trim_start_matches("~/")))
        .collect();
    fn walk(
        base: &Path,
        relative: &Path,
        declared: &[PathBuf],
        budget: &mut usize,
        out: &mut Vec<HostPath>,
    ) -> Result<()> {
        let dir = match crate::safe_fs::open_dir_beneath(base, relative, false) {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        use std::os::fd::AsRawFd;
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let entry = entry?;
            let rel = relative.join(entry.file_name());
            if declared.iter().any(|p| rel.starts_with(p)) {
                continue;
            }
            if declared.iter().any(|p| p.starts_with(&rel)) && entry.file_type()?.is_dir() {
                walk(base, &rel, declared, budget, out)?;
            } else {
                out.push(HostPath::from_path(&base.join(rel)));
            }
        }
        Ok(())
    }
    let mut out = vec![];
    walk(base, Path::new(""), &declared, &mut 512, &mut out)?;
    out.sort();
    Ok(out)
}

impl Plan {
    fn rule(
        &mut self,
        target: &Path,
        backing: Option<&Path>,
        behavior: &str,
        source: &str,
        reason: &str,
    ) {
        self.view.rules.push(PathRule {
            target: HostPath::from_path(target),
            backing: backing.map(HostPath::from_path),
            behavior: behavior.into(),
            source: source.into(),
            reason: reason.into(),
        });
    }
    /// A workspace session's repositories (§12): a pin at or inside one is not bound — its clone
    /// takes the repository's place — and the view says so.
    pub fn set_workspaces(&mut self, binds: &[WorkspaceBind]) -> Result<()> {
        let sources: Vec<PathBuf> = binds.iter().map(|b| b.source.to_path()).collect::<Result<_>>()?;
        let covered = |t: &HostPath| t.to_path().is_ok_and(|t| sources.iter().any(|s| t.starts_with(s)));
        self.bindings.retain(|b| b.declaration.is_some() || !covered(&b.target));
        self.view.rules.retain(|r| r.behavior != "pin" || !covered(&r.target));
        for b in binds {
            self.rule(
                &b.source.to_path()?,
                Some(&b.clone.to_path()?),
                "workspace",
                "session workspace",
                "This session's own copy of the repository; the person's checkout changes only when they apply its work.",
            );
        }
        self.workspaces = binds.to_vec();
        Ok(())
    }
    pub fn add_fragments(&mut self, ro: &[PathBuf], masks: &[PathBuf]) {
        for path in ro {
            self.rule(
                path,
                Some(path),
                "read_only",
                "repository fragment",
                "Repository restriction applied after writable bindings.",
            );
        }
        for path in masks {
            self.rule(
                path,
                None,
                "mask",
                "repository fragment",
                "Repository restriction applied after writable bindings.",
            );
        }
    }
    pub fn binds(&self) -> Result<Vec<(PathBuf, PathBuf)>> {
        self.bindings
            .iter()
            .map(|b| Ok((b.source.to_path()?, b.target.to_path()?)))
            .collect()
    }
    pub fn state_paths(&self) -> Result<Vec<PathBuf>> {
        self.bindings
            .iter()
            .filter(|b| b.declaration.is_some())
            .map(|b| b.target.to_path())
            .collect()
    }
    /// Compare enforcement, ignoring the informational inventory of unused files.
    pub fn same_mounts(&self, other: &Self) -> bool {
        self.policy_paths == other.policy_paths
            && self.credentials == other.credentials
            && self.normal_profile == other.normal_profile
            && self.bindings == other.bindings
            && self.agent_masks == other.agent_masks
            && self.view.rules == other.view.rules
            && self.workspaces == other.workspaces
    }
    pub fn prepare(&self) -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        let profile = self.profile.to_path()?;
        let parent = profile.parent().context("profile parent")?;
        if self.ghost || self.bindings.iter().any(|b| b.declaration.is_some()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
            crate::safe_fs::owned_dir(parent, rustix::process::getuid().as_raw(), false)?;
            crate::safe_fs::owned_dir(&profile, rustix::process::getuid().as_raw(), true)?;
        }
        for binding in &self.bindings {
            let source = binding.source.to_path()?;
            if let Some(entry) = &binding.declaration {
                let rel = source
                    .strip_prefix(&profile)
                    .context("agent source outside its profile")?;
                match entry.kind {
                    HomePathKind::Directory => {
                        crate::safe_fs::open_dir_beneath(&profile, rel, true)?;
                    }
                    HomePathKind::File => {
                        crate::safe_fs::initialize_private_beneath(
                            &profile,
                            rel,
                            entry.initial_content.as_deref().unwrap_or("").as_bytes(),
                        )?;
                    }
                }
            } else if self
                .view
                .rules
                .iter()
                .any(|r| r.behavior == "private" && r.target == binding.target)
            {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&source)?;
                crate::safe_fs::owned_dir(&source, rustix::process::getuid().as_raw(), false)?;
            }
        }
        if self.ghost {
            crate::credentials::seed(&self.credentials, &self.normal_profile.to_path()?, &profile)?;
        }
        Ok(())
    }
    pub fn apply(&self, spec: &mut crate::jail::JailSpec) -> Result<()> {
        spec.binds = self.binds()?;
        spec.workspace_mask = self.workspaces.iter().map(|w| Ok::<_, anyhow::Error>((w.source.to_path()?, w.clone.to_path()?, w.objects.to_path()?))).collect::<Result<_>>()?;
        spec.policy_paths = Some(self.policy_paths.clone());
        spec.agent_masks = self
            .agent_masks
            .iter()
            .map(|m| Ok((m.path.to_path()?, m.kind)))
            .collect::<Result<_>>()?;
        spec.agent_binds = self
            .bindings
            .iter()
            .filter(|b| b.declaration.is_some())
            .map(|b| Ok((b.target.to_path()?, b.declaration.as_ref().unwrap().kind)))
            .collect::<Result<_>>()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        home: PathBuf,
        state: PathBuf,
        registry: BTreeMap<String, Agent>,
    }
    impl Fixture {
        fn new() -> Self {
            assert!(
                std::env::var_os("XDG_STATE_HOME").is_none(),
                "run storage tests with XDG_STATE_HOME unset"
            );
            let root =
                std::env::temp_dir().join(format!("sill-storage-{:032x}", rand::random::<u128>()));
            let home = root.join("home");
            std::fs::create_dir_all(&home).unwrap();
            Self {
                state: root.join("state"),
                root,
                home,
                registry: crate::agents::builtin()
                    .into_iter()
                    .map(|a| (a.id.clone(), a))
                    .collect(),
            }
        }
        fn plan(&self, id: &str, ghost: bool) -> Plan {
            resolve(
                &self.registry[id],
                &self.registry,
                &Policy::builtin_default(),
                &self.state,
                &self.home,
                &[],
                ghost,
            )
            .unwrap()
        }
        fn put(&self, path: &Path, bytes: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn cargo_writes_and_stray_profile_storage_do_not_change_any_agents_mounts() {
        let f = Fixture::new();
        let cargo = f.home.join(".cargo/registry/src/host-dependency/lib.rs");
        f.put(&cargo, "host dependency");
        let before_codex = f.plan("codex", false);
        let before_claude = f.plan("claude", false);
        let stray = crate::session::agent_home(&f.home, "codex").join(".cargo/.global-cache");
        f.put(&stray, "incidental tool state");
        f.put(&f.home.join(".cargo/new-file"), "a new tool file");
        for (id, before) in [("codex", before_codex), ("claude", before_claude)] {
            let after = f.plan(id, false);
            assert!(
                before.same_mounts(&after),
                "ordinary writes changed {id}'s rules"
            );
            assert!(
                !after
                    .agent_masks
                    .iter()
                    .any(|m| m.path == HostPath::from_path(&f.home.join(".cargo")))
            );
            assert!(
                !after
                    .binds()
                    .unwrap()
                    .iter()
                    .any(|(_, target)| target == &f.home.join(".cargo"))
            );
            after.prepare().unwrap();
        }
        assert_eq!(std::fs::read_to_string(cargo).unwrap(), "host dependency");
        assert_eq!(
            std::fs::read_to_string(&stray).unwrap(),
            "incidental tool state"
        );
        assert_eq!(
            f.plan("codex", false).view.unused,
            vec![HostPath::from_path(stray.parent().unwrap())]
        );
    }

    #[test]
    fn resolving_is_read_only_and_recorded_rules_survive_definition_edits() {
        let mut f = Fixture::new();
        let before = f.plan("claude", false);
        assert!(!f.home.join(".local").exists());
        assert!(!f.state.exists());
        let bytes = serde_json::to_vec(&before).unwrap();
        f.registry
            .get_mut("claude")
            .unwrap()
            .home_paths
            .as_mut()
            .unwrap()
            .push(HomePath::directory("~/.config/new-agent-path"));
        let after = f.plan("claude", false);
        let recorded: Plan = serde_json::from_slice(&bytes).unwrap();
        assert!(recorded.same_mounts(&before));
        assert!(!recorded.same_mounts(&after));
        assert!(!f.home.join(".local").exists());
    }

    #[test]
    fn nested_roots_keep_full_names_and_existing_files_are_not_initialized_again() {
        let mut f = Fixture::new();
        let a = f.registry.get_mut("codex").unwrap();
        a.home_paths = Some(vec![
            HomePath::directory("~/.config/example"),
            HomePath::directory("~/.local/share/example"),
            HomePath::file("~/.example.json", "{}"),
        ]);
        a.home_env = None;
        a.login_files.clear();
        let plan = f.plan("codex", false);
        f.put(&f.home.join(".config/sibling/config"), "host sibling");
        plan.prepare().unwrap();
        let profile = plan.profile.to_path().unwrap();
        assert!(profile.join(".config/example").is_dir());
        assert!(profile.join(".local/share/example").is_dir());
        assert!(!profile.join("example").exists());
        f.put(&profile.join(".example.json"), "saved settings");
        plan.prepare().unwrap();
        assert_eq!(
            std::fs::read_to_string(profile.join(".example.json")).unwrap(),
            "saved settings"
        );
        assert_eq!(
            std::fs::read_to_string(f.home.join(".config/sibling/config")).unwrap(),
            "host sibling"
        );
    }

    #[test]
    fn incomplete_definitions_are_distinct_from_explicit_stateless_agents() {
        let mut f = Fixture::new();
        let a = f.registry.get_mut("codex").unwrap();
        a.home_paths = None;
        a.login_files.clear();
        a.home_env = None;
        let error = resolve(
            a,
            &BTreeMap::new(),
            &Policy::builtin_default(),
            &f.state,
            &f.home,
            &[],
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("needs directory setup"));
        assert!(!f.home.join(".local").exists());
        a.home_paths = Some(vec![]);
        assert!(
            resolve(
                a,
                &BTreeMap::new(),
                &Policy::builtin_default(),
                &f.state,
                &f.home,
                &[],
                false
            )
            .unwrap()
            .state_paths()
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn ghost_imports_credentials_only_and_normal_sessions_share_the_same_profile() {
        let f = Fixture::new();
        let profile = crate::session::agent_home(&f.home, "codex");
        f.put(&profile.join(".codex/auth.json"), "{\"token\":\"fixture\"}");
        f.put(&profile.join(".codex/config.toml"), "hooks");
        f.put(&profile.join(".codex/sessions/history"), "history");
        let ghost = f.plan("codex", true);
        ghost.prepare().unwrap();
        let private = ghost.profile.to_path().unwrap();
        assert!(private.join(".codex/auth.json").is_file());
        assert!(!private.join(".codex/config.toml").exists());
        assert!(!private.join(".codex/sessions").exists());
        let next = resolve(
            &f.registry["codex"],
            &f.registry,
            &Policy::builtin_default(),
            &f.root.join("other-session"),
            &f.home,
            &[],
            false,
        )
        .unwrap();
        assert_eq!(f.plan("codex", false).profile, next.profile);
    }

    #[test]
    fn storage_rejects_links_wrong_types_and_pin_conflicts() {
        let f = Fixture::new();
        let profile = crate::session::agent_home(&f.home, "codex");
        std::fs::create_dir_all(&profile).unwrap();
        let outside = f.root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, profile.join(".codex")).unwrap();
        assert!(f.plan("codex", false).prepare().is_err());
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        std::fs::remove_file(profile.join(".codex")).unwrap();
        std::fs::write(profile.join(".codex"), "wrong type").unwrap();
        assert!(f.plan("codex", false).prepare().is_err());
        assert!(
            resolve(
                &f.registry["codex"],
                &f.registry,
                &Policy::builtin_default(),
                &f.state,
                &f.home,
                std::slice::from_ref(&f.home),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn equal_application_paths_have_separate_profiles_and_frozen_credentials() {
        let mut f = Fixture::new();
        let mut second = f.registry["codex"].clone();
        second.id = "codex-other".into();
        f.registry.insert(second.id.clone(), second);
        let first = f.plan("codex", true);
        let second = f.plan("codex-other", false);
        assert_eq!(first.state_paths().unwrap(), second.state_paths().unwrap());
        assert_ne!(first.profile, second.profile);
        f.registry.get_mut("codex").unwrap().login_files = vec![".codex/new-token.json".into()];
        assert!(!first.same_mounts(&f.plan("codex", true)));
        let normal = first.normal_profile.to_path().unwrap();
        f.put(&normal.join(".codex/auth.json"), "{\"old-declared\":true}");
        f.put(
            &normal.join(".codex/new-token.json"),
            "{\"new-declared\":true}",
        );
        first.prepare().unwrap();
        let ghost = first.profile.to_path().unwrap();
        assert!(ghost.join(".codex/auth.json").is_file());
        assert!(!ghost.join(".codex/new-token.json").exists());
    }

    #[test]
    fn development_permissions_do_not_change_directory_ownership() {
        let f = Fixture::new();
        let before = f.plan("codex", false);
        let mut policy = Policy::builtin_default();
        policy.permissions.private_bus = crate::permissions::Rule::On;
        policy.permissions.test_desktop = crate::permissions::Rule::On;
        let after = resolve(
            &f.registry["codex"],
            &f.registry,
            &policy,
            &f.state,
            &f.home,
            &[],
            false,
        )
        .unwrap();
        assert!(before.same_mounts(&after));
        let development =
            resolve_development(&f.registry, &policy, &f.state, &f.home, &[]).unwrap();
        assert!(development.state_paths().unwrap().is_empty());
        development.prepare().unwrap();
        assert!(!development.profile.to_path().unwrap().exists());
        assert!(!development.agent_masks.is_empty());
    }

    #[test]
    fn changes_inspection_skips_what_a_workspace_clone_hides() {
        // the overlay's entries below a workspace's repository are behind the clone's bind: no
        // change against the host (they were listed, and a revert went through the bind, 2026-11-09)
        let f = Fixture::new();
        let mut storage = f.plan("codex", false);
        let repo = f.home.join("GIT/repo");
        storage.workspaces.push(WorkspaceBind {
            source: HostPath::from_path(&repo),
            clone: HostPath::from_path(&f.root.join("clone")),
            objects: HostPath::from_path(&repo.join(".git/objects")),
        });
        let plan = crate::changes::session_plan(
            Policy::builtin_default(),
            f.state.clone(),
            &f.home,
            rustix::process::getuid().as_raw(),
            "fixture",
            &storage,
        )
        .unwrap();
        assert!(plan.direct.contains(&repo));
    }

    #[test]
    fn changes_inspection_uses_recorded_masks_without_preparing_or_rescanning_storage() {
        let f = Fixture::new();
        let first = f.home.join("hidden-first");
        let second = f.home.join("hidden-second");
        f.put(&first, "fixture secret");
        let mut policy = Policy::builtin_default();
        policy
            .paths
            .mask
            .push(f.home.join("hidden-*").display().to_string());
        let storage = resolve(
            &f.registry["codex"],
            &f.registry,
            &policy,
            &f.state,
            &f.home,
            &[],
            false,
        )
        .unwrap();
        std::fs::remove_file(&first).unwrap();
        f.put(&second, "a later host file");
        let plan = crate::changes::session_plan(
            policy,
            f.state.clone(),
            &f.home,
            rustix::process::getuid().as_raw(),
            "fixture",
            &storage,
        )
        .unwrap();
        assert!(plan.masked.contains(&first));
        assert!(
            !plan.masked.contains(&second),
            "current glob matches must not rewrite launch evidence"
        );
        assert!(!storage.profile.to_path().unwrap().exists());
        assert!(
            !f.state.exists(),
            "Changes inspection created session storage"
        );
    }
}
