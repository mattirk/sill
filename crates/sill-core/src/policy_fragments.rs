//! Restrictive repository policy fragments. The resolved policy retains base path rules;
//! fragment paths are separate so the mount planner can enforce them after writable pins.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustix::fs::{FileType, Mode, OFlags, RawMode, ResolveFlags, fstat, open, openat2};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::policy::{NetMode, Policy};

pub const MAX_FRAGMENT_BYTES: usize = 64 * 1024;
pub const MAX_TOTAL_BYTES: usize = 1024 * 1024;
pub const MAX_ROOTS: usize = 128;
const FRAGMENT: &str = ".sill/policy.toml";
const RESOLVE: ResolveFlags = ResolveFlags::BENEATH.union(ResolveFlags::NO_SYMLINKS);

// Internal snapshots must retain exact Linux path bytes, just like public HostPath DTOs.
// Accept the string form used by early snapshots too.
#[derive(Deserialize)]
#[serde(untagged)]
enum EncodedPath {
    Bytes(Vec<u8>),
    Text(PathBuf),
}
impl EncodedPath {
    fn into_path(self) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        match self {
            Self::Bytes(bytes) => std::ffi::OsString::from_vec(bytes).into(),
            Self::Text(path) => path,
        }
    }
}
mod path_bytes {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        path: &Path,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_bytes(path.as_os_str().as_encoded_bytes())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<PathBuf, D::Error> {
        Ok(EncodedPath::deserialize(deserializer)?.into_path())
    }
}
mod paths_bytes {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        paths: &[PathBuf],
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        paths
            .iter()
            .map(|path| path.as_os_str().as_encoded_bytes())
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Vec<PathBuf>, D::Error> {
        Ok(Vec::<EncodedPath>::deserialize(deserializer)?
            .into_iter()
            .map(EncodedPath::into_path)
            .collect())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    #[serde(with = "path_bytes")]
    pub root: PathBuf,
    #[serde(with = "path_bytes")]
    pub path: PathBuf,
    pub text: String,
    pub sha256: String,
    pub ignored: bool,
    /// Present when an explicitly ignored source could not be read safely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Suggestion {
    pub host: String,
    #[serde(with = "path_bytes")]
    pub source: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    /// Base path rules stay here; the additional paths below complete the effective rules.
    pub policy: Policy,
    #[serde(with = "paths_bytes")]
    pub fragment_ro: Vec<PathBuf>,
    #[serde(with = "paths_bytes")]
    pub fragment_masks: Vec<PathBuf>,
    pub sources: Vec<Source>,
    pub suggestions: Vec<Suggestion>,
    /// Hash of base policy, canonical roots and raw source inputs, including missing sources.
    pub fingerprint: String,
    /// Remembered session capability choices this build consulted, with their outcome.
    #[serde(default)]
    pub session_permissions: Vec<crate::permissions::SessionChoice>,
    /// Session-only host approvals included in this build's allow-list.
    #[serde(default)]
    pub session_hosts: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fragment {
    #[serde(default)]
    permissions: std::collections::BTreeMap<String, crate::permissions::Rule>,
    #[serde(default)]
    paths: FragmentPaths,
    #[serde(default)]
    network: FragmentNetwork,
    #[serde(default)]
    limits: FragmentLimits,
    #[serde(default)]
    processes: FragmentProcesses,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FragmentPaths {
    #[serde(default)]
    ro: Vec<String>,
    #[serde(default)]
    mask: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FragmentNetwork {
    mode: Option<Offline>,
    #[serde(default)]
    allow: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Offline {
    None,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FragmentLimits {
    cpu_weight: Option<u32>,
    io_weight: Option<u32>,
    memory_high: Option<String>,
    memory_max: Option<String>,
    tasks_max: Option<u32>,
    idle_timeout_minutes: Option<u32>,
    agent_idle_minutes: Option<u32>,
    required: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FragmentProcesses {
    expose: Option<Vec<String>>,
}

/// Canonical, distinct folder pins. File pins are not policy roots. Pinned roots may themselves
/// be aliases, but fragment and rule paths below them must not contain symlinks.
pub fn roots(pins: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if pins.len() > MAX_ROOTS {
        bail!("at most {MAX_ROOTS} pinned paths may contribute policy fragments");
    }
    let mut roots = BTreeSet::new();
    for pin in pins {
        let path = pin
            .canonicalize()
            .with_context(|| format!("resolve pinned path {}", pin.display()))?;
        if path.is_dir() {
            roots.insert(path);
        }
    }
    Ok(roots.into_iter().collect())
}

/// Validate a fragment independently of any base policy. Paths must already exist below root.
pub fn check_fragment(text: &str, root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolve repository {}", root.display()))?;
    let f = parse(text)?;
    validate(&f, &root)
}

fn parse(text: &str) -> Result<Fragment> {
    if text.len() > MAX_FRAGMENT_BYTES {
        bail!("fragment exceeds {MAX_FRAGMENT_BYTES} bytes");
    }
    toml::from_str(text).context("parse repository policy fragment")
}

fn open_beneath(root: &Path, relative: &Path, flags: OFlags) -> Result<std::os::fd::OwnedFd> {
    let root_fd = open(
        root,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("open repository {}", root.display()))?;
    let flags = flags
        | OFlags::CLOEXEC
        | if flags.contains(OFlags::PATH) {
            OFlags::empty()
        } else {
            OFlags::NONBLOCK
        };
    openat2(&root_fd, relative, flags, Mode::empty(), RESOLVE).with_context(|| {
        format!(
            "open {} below {} without symlinks",
            relative.display(),
            root.display()
        )
    })
}

/// Open all components in one beneath/no-symlinks lookup. A FIFO never stalls a resolver.
fn read_source(root: &Path) -> Result<Option<String>> {
    let fd = match open_beneath(root, Path::new(FRAGMENT), OFlags::RDONLY) {
        Ok(fd) => fd,
        Err(e) if e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    if FileType::from_raw_mode(fstat(&fd)?.st_mode as RawMode) != FileType::RegularFile {
        bail!("{} is not a regular file", root.join(FRAGMENT).display());
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(MAX_FRAGMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FRAGMENT_BYTES {
        bail!("fragment exceeds {MAX_FRAGMENT_BYTES} bytes");
    }
    Ok(Some(
        String::from_utf8(bytes).context("fragment is not UTF-8")?,
    ))
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Inputs {
    roots: Vec<PathBuf>,
    values: Vec<std::result::Result<Option<String>, String>>,
    fingerprint: String,
}

fn inputs(base: &Policy, pins: &[PathBuf]) -> Result<Inputs> {
    let roots = roots(pins)?;
    let mut hash = Sha256::new();
    let base = serde_json::to_vec(base)?;
    hash.update((base.len() as u64).to_le_bytes());
    hash.update(&base);
    let mut values = Vec::new();
    let mut total = 0usize;
    for root in &roots {
        let name = root.as_os_str().as_encoded_bytes();
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
        let value = read_source(root).map_err(|e| format!("{e:#}"));
        let encoded = serde_json::to_vec(&value)?;
        total += encoded.len();
        if total > MAX_TOTAL_BYTES {
            bail!("repository policy inputs exceed {MAX_TOTAL_BYTES} bytes");
        }
        hash.update((encoded.len() as u64).to_le_bytes());
        hash.update(encoded);
        values.push(value);
    }
    Ok(Inputs {
        roots,
        values,
        fingerprint: hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    })
}

/// Reads safely but does not parse source TOML: invalid edits must mark an existing jail stale.
pub fn input_fingerprint(base: &Policy, pins: &[PathBuf]) -> Result<String> {
    Ok(inputs(base, pins)?.fingerprint)
}

/// Resolve once per jail build. Ignore roots are explicit human exceptions for this call only;
/// they are never persisted as input to future resolutions. Every ignored source is recorded.
pub fn resolve(base: &Policy, pins: &[PathBuf], ignore_roots: &[PathBuf]) -> Result<Resolution> {
    let input = inputs(base, pins)?;
    let ignored = roots(ignore_roots)?;
    for root in &ignored {
        if !input.roots.contains(root) {
            bail!("ignored repository {} is not pinned", root.display());
        }
    }
    let mut out = Resolution {
        policy: base.clone(),
        fragment_ro: vec![],
        fragment_masks: vec![],
        sources: vec![],
        suggestions: vec![],
        fingerprint: input.fingerprint,
        session_permissions: vec![],
        session_hosts: vec![],
    };
    for (root, value) in input.roots.into_iter().zip(input.values) {
        let path = root.join(FRAGMENT);
        let ignored = ignored.contains(&root);
        let (text, error) = match value {
            Ok(Some(text)) => (text, None),
            Ok(None) if !ignored => continue,
            Ok(None) => (String::new(), Some("fragment absent at build time".into())),
            Err(error) if ignored => (String::new(), Some(error)),
            Err(error) => bail!("{}: {error}", path.display()),
        };
        let source = Source {
            root: root.clone(),
            path: path.clone(),
            sha256: digest(text.as_bytes()),
            text,
            ignored,
            error,
        };
        if !ignored {
            let fragment = parse(&source.text).with_context(|| format!("{}", path.display()))?;
            validate(&fragment, &root).with_context(|| format!("{}", path.display()))?;
            apply(&fragment, &root, &path, &mut out)
                .with_context(|| format!("{}", path.display()))?;
        }
        out.sources.push(source);
    }
    out.fragment_ro.sort();
    out.fragment_ro.dedup();
    out.fragment_masks.sort();
    out.fragment_masks.dedup();
    Ok(out)
}

fn plain_path(root: &Path, value: &str) -> Result<PathBuf> {
    let relative = Path::new(value);
    if value.is_empty()
        || value.contains(['*', '?', '[', ']'])
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!(
            "path {value:?} must be a plain repository-relative path without traversal or patterns"
        );
    }
    let fd = open_beneath(root, relative, OFlags::PATH)?;
    let kind = FileType::from_raw_mode(fstat(&fd)?.st_mode as RawMode);
    if !matches!(kind, FileType::Directory | FileType::RegularFile) {
        bail!("path {value:?} is not a regular file or directory");
    }
    let path = relative
        .components()
        .fold(root.to_path_buf(), |mut path, c| {
            if let Component::Normal(name) = c {
                path.push(name);
            }
            path
        });
    Ok(path)
}

fn validate(f: &Fragment, root: &Path) -> Result<()> {
    let known = crate::permissions::Permissions::default();
    for (capability, rule) in &f.permissions {
        if !known.rules().iter().any(|(name, _)| *name == capability)
            || *rule != crate::permissions::Rule::Forbidden {
            bail!("repository permission {capability:?} must name a known capability and be forbidden");
        }
    }
    if f.paths.ro.len() + f.paths.mask.len() > 256 {
        bail!("at most 256 path rules per fragment");
    }
    for path in f.paths.ro.iter().chain(&f.paths.mask) {
        plain_path(root, path)?;
    }
    if f.network.allow.len() > 128 {
        bail!("at most 128 host suggestions per fragment");
    }
    for host in &f.network.allow {
        // Concrete default-port host names only; the existing Allow controls use the same format.
        if !crate::proxy::valid_hostname(host) {
            bail!("network.allow entry {host:?} must be a concrete host without wildcard or port");
        }
    }
    for (name, weight) in [
        ("cpu_weight", f.limits.cpu_weight),
        ("io_weight", f.limits.io_weight),
    ] {
        if weight.is_some_and(|n| !(1..=10_000).contains(&n)) {
            bail!("limits.{name} must be 1..10000");
        }
    }
    if f.limits.tasks_max == Some(0) {
        bail!("limits.tasks_max must be positive");
    }
    if f.limits.required == Some(false) {
        bail!("limits.required=false would relax enforcement");
    }
    for size in [&f.limits.memory_high, &f.limits.memory_max]
        .into_iter()
        .flatten()
    {
        memory_bytes(size)?;
    }
    if let Some(ports) = &f.processes.expose {
        port_set(ports)?;
    }
    Ok(())
}

/// Parse finite sizes exactly, without float saturation or rounding; zero is not a usable cap.
/// Accepted units match complete policies (K/M/G/T, binary multiples). Unlimited sorts last.
fn memory_bytes(value: &str) -> Result<u64> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("max") || value.eq_ignore_ascii_case("infinity") {
        return Ok(u64::MAX);
    }
    let (number, factor) = match value.as_bytes().last() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1u64 << 10),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1u64 << 20),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1u64 << 30),
        Some(b't' | b'T') => (&value[..value.len() - 1], 1u64 << 40),
        _ => (value, 1),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 9
    {
        bail!("invalid memory size {value:?}; use positive bytes or K/M/G/T units");
    }
    let whole: u64 = whole.parse().context("memory size overflow")?;
    let integer = whole.checked_mul(factor).context("memory size overflow")?;
    let decimal = if fraction.is_empty() {
        0
    } else {
        let denominator = 10u64.pow(fraction.len() as u32);
        let numerator = fraction
            .parse::<u64>()?
            .checked_mul(factor)
            .context("memory size overflow")?;
        if numerator % denominator != 0 {
            bail!("memory size {value:?} is not an exact byte count");
        }
        numerator / denominator
    };
    let bytes = integer
        .checked_add(decimal)
        .context("memory size overflow")?;
    if bytes == 0 || bytes == u64::MAX {
        bail!("memory size {value:?} is not a usable positive cap");
    }
    Ok(bytes)
}

fn timeout_min(a: u32, b: u32) -> u32 {
    match (a, b) {
        (0, n) | (n, 0) => n,
        (a, b) => a.min(b),
    }
}

fn merge_memory(current: &mut String, proposed: &str) -> Result<()> {
    let bytes = memory_bytes(current)?.min(memory_bytes(proposed)?);
    *current = if bytes == u64::MAX {
        "infinity".into()
    } else {
        bytes.to_string()
    };
    Ok(())
}

fn port_set(specs: &[String]) -> Result<BTreeSet<u16>> {
    if specs.len() > 256 {
        bail!("at most 256 port ranges");
    }
    let mut ports = BTreeSet::new();
    for spec in specs {
        let (a, b) = match spec.split_once('-') {
            Some((a, b)) => (a.parse::<u16>(), b.parse::<u16>()),
            None => (spec.parse::<u16>(), spec.parse::<u16>()),
        };
        let (a, b) = (
            a.with_context(|| format!("invalid port range {spec:?}"))?,
            b.with_context(|| format!("invalid port range {spec:?}"))?,
        );
        if a == 0 || a > b {
            bail!("invalid port range {spec:?}");
        }
        ports.extend(a..=b);
    }
    Ok(ports)
}

fn ranges(ports: BTreeSet<u16>) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = None;
    let mut last = 0;
    for port in ports {
        if start.is_some() && port != last + 1 {
            let a = start.take().unwrap();
            out.push(if a == last {
                format!("{a}")
            } else {
                format!("{a}-{last}")
            });
        }
        if start.is_none() {
            start = Some(port);
        }
        last = port;
    }
    if let Some(a) = start {
        out.push(if a == last {
            format!("{a}")
        } else {
            format!("{a}-{last}")
        });
    }
    out
}

fn apply(f: &Fragment, root: &Path, source: &Path, out: &mut Resolution) -> Result<()> {
    use crate::permissions::Rule;
    for capability in f.permissions.keys() {
        match capability.as_str() {
            "test_desktop" => out.policy.permissions.test_desktop = Rule::Forbidden,
            "private_bus" => out.policy.permissions.private_bus = Rule::Forbidden,
            "gpu" => out.policy.permissions.gpu = Rule::Forbidden,
            "host_control" => out.policy.permissions.host_control = Rule::Forbidden,
            "public_web" => {
                out.policy.permissions.public_web = Rule::Forbidden;
                if out.policy.network.mode == NetMode::All { out.policy.network.mode = NetMode::Allow; }
            }
            _ => unreachable!("validated permission name"),
        }
    }
    for path in &f.paths.ro {
        out.fragment_ro.push(plain_path(root, path)?);
    }
    for path in &f.paths.mask {
        out.fragment_masks.push(plain_path(root, path)?);
    }
    if f.network.mode.is_some() {
        out.policy.network.mode = NetMode::None;
        // Proxy local exceptions are tested before mode; none must really mean none.
        out.policy.network.allow.clear();
        out.policy.network.allow_local.clear();
    }
    if let Some(proposed) = &f.processes.expose {
        let base = port_set(&out.policy.processes.expose)?;
        let fragment = port_set(proposed)?;
        out.policy.processes.expose = ranges(base.intersection(&fragment).copied().collect());
    }
    for host in &f.network.allow {
        out.suggestions.push(Suggestion {
            host: host.to_ascii_lowercase(),
            source: source.into(),
        });
    }
    let limits = &mut out.policy.limits;
    let f = &f.limits;
    if let Some(n) = f.cpu_weight {
        limits.cpu_weight = limits.cpu_weight.min(n);
    }
    if let Some(n) = f.io_weight {
        limits.io_weight = limits.io_weight.min(n);
    }
    if let Some(n) = f.tasks_max {
        limits.tasks_max = limits.tasks_max.min(n);
    }
    if let Some(n) = &f.memory_high {
        merge_memory(&mut limits.memory_high, n)?;
    }
    if let Some(n) = &f.memory_max {
        merge_memory(&mut limits.memory_max, n)?;
    }
    if let Some(n) = f.idle_timeout_minutes {
        limits.idle_timeout_minutes = timeout_min(limits.idle_timeout_minutes, n);
    }
    if let Some(n) = f.agent_idle_minutes {
        limits.agent_idle_minutes = timeout_min(limits.agent_idle_minutes, n);
    }
    if f.required == Some(true)
        || f.cpu_weight.is_some()
        || f.io_weight.is_some()
        || f.memory_high.is_some()
        || f.memory_max.is_some()
        || f.tasks_max.is_some()
    {
        limits.required = true;
    }
    // Clamping memory.high to memory.max is needed when only a max cap was supplied.
    if f.memory_high.is_some() || f.memory_max.is_some() {
        let max = limits.memory_max.clone();
        merge_memory(&mut limits.memory_high, &max)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Repo(PathBuf);
    impl Repo {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "sill-policy-fragment-{:032x}",
                rand::random::<u128>()
            ));
            std::fs::create_dir_all(root.join(".sill")).unwrap();
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/lib.rs"), "content").unwrap();
            Self(root)
        }
        fn write(&self, text: &str) {
            std::fs::write(self.0.join(FRAGMENT), text).unwrap();
        }
        fn resolve(&self, base: &Policy) -> Result<Resolution> {
            resolve(base, std::slice::from_ref(&self.0), &[])
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn absent_and_empty_fragments_impose_no_defaults() {
        let repo = Repo::new();
        let mut base = Policy::builtin_default();
        base.limits.required = false;
        base.network.mode = NetMode::All;
        let absent = repo.resolve(&base).unwrap();
        assert!(absent.sources.is_empty());
        assert!(absent.fragment_ro.is_empty());
        repo.write("");
        let empty = repo.resolve(&base).unwrap();
        assert_eq!(
            serde_json::to_value(&empty.policy).unwrap(),
            serde_json::to_value(base).unwrap()
        );
        assert_ne!(absent.fingerprint, empty.fingerprint);
    }

    #[test]
    fn strict_parser_refuses_grants_and_deferred_fields() {
        let repo = Repo::new();
        for text in [
            "name='project'",
            "[paths]\ndirect=['src']",
            "[paths]\nledger_ignore=['target']",
            "[paths]\nhidden_mounts=['src']",
            "[jail]\nx11=true",
            "[jail]\nseccomp=false",
            "[network]\nmode='all'",
            "[network]\nmode='allow'",
            "[network]\nallow_local=['localhost']",
            "[limits]\nrequired=false",
            "[processes]\nunknown=true",
            "unknown=123",
            "[",
        ] {
            assert!(check_fragment(text, &repo.0).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn contained_paths_are_separate_and_plain() {
        let repo = Repo::new();
        repo.write("[paths]\nro=['./src', 'src']\nmask=['src/lib.rs']");
        let resolved = repo.resolve(&Policy::builtin_default()).unwrap();
        assert_eq!(resolved.fragment_ro, vec![repo.0.join("src")]);
        assert_eq!(resolved.fragment_masks, vec![repo.0.join("src/lib.rs")]);
        assert_eq!(resolved.policy.paths.ro, Policy::builtin_default().paths.ro);
        for path in [
            "../outside",
            "/etc/passwd",
            "~/secrets",
            "src/*",
            "missing",
            "src/../../etc",
        ] {
            let text = format!("[paths]\nro=[{path:?}]");
            assert!(check_fragment(&text, &repo.0).is_err(), "accepted {path}");
        }
    }

    #[test]
    fn source_and_rule_symlinks_are_refused() {
        let repo = Repo::new();
        symlink("src/lib.rs", repo.0.join("link")).unwrap();
        assert!(check_fragment("[paths]\nro=['link']", &repo.0).is_err());
        symlink("src", repo.0.join("linked-dir")).unwrap();
        assert!(check_fragment("[paths]\nmask=['linked-dir/lib.rs']", &repo.0).is_err());
        symlink("../src/lib.rs", repo.0.join(FRAGMENT)).unwrap();
        assert!(repo.resolve(&Policy::builtin_default()).is_err());
        std::fs::remove_file(repo.0.join(FRAGMENT)).unwrap();
        std::fs::remove_dir(repo.0.join(".sill")).unwrap();
        symlink("src", repo.0.join(".sill")).unwrap();
        assert!(repo.resolve(&Policy::builtin_default()).is_err());
    }

    #[test]
    fn special_files_cannot_stall_source_reads() {
        let repo = Repo::new();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &repo.0.join(FRAGMENT),
            FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        let error = format!(
            "{:#}",
            repo.resolve(&Policy::builtin_default()).unwrap_err()
        );
        assert!(error.contains("regular file"), "{error}");
    }

    #[test]
    fn suggestions_do_not_grant_and_offline_clears_local_exceptions() {
        let repo = Repo::new();
        let mut base = Policy::builtin_default();
        base.network.allow_local = vec!["localhost".into()];
        repo.write("[network]\nallow=['EXAMPLE.com', '127.0.0.1']");
        let resolved = repo.resolve(&base).unwrap();
        assert_eq!(resolved.policy.network.allow, base.network.allow);
        assert_eq!(resolved.suggestions[0].host, "example.com");
        assert_eq!(resolved.suggestions[0].source, repo.0.join(FRAGMENT));
        for bad in [
            "*.example.com",
            "example.com:8443",
            "example.com/path",
            "a\nb",
            "",
        ] {
            let text = format!("[network]\nallow=[{bad:?}]");
            assert!(check_fragment(&text, &repo.0).is_err());
        }
        repo.write("[network]\nmode='none'\nallow=['example.com']");
        let offline = repo.resolve(&base).unwrap();
        assert_eq!(offline.policy.network.mode, NetMode::None);
        assert!(offline.policy.network.allow_local.is_empty());
        assert!(offline.policy.network.allow.is_empty());
        let rules = crate::proxy::Rules::from(&offline.policy.network);
        assert!(matches!(
            rules.decide_name("localhost", 443, 443),
            crate::proxy::Decision::Deny(_)
        ));
    }

    #[test]
    fn minima_are_order_independent_and_enforceable() {
        let a = Repo::new();
        let b = Repo::new();
        a.write("[limits]\ncpu_weight=10\nmemory_max='2G'\ntasks_max=400\nidle_timeout_minutes=0\nagent_idle_minutes=2\n[processes]\nexpose=['3000-3500']");
        b.write("[limits]\nio_weight=12\nmemory_high='512M'\ntasks_max=100\nidle_timeout_minutes=15\n[processes]\nexpose=['3400-3600']");
        let mut base = Policy::builtin_default();
        base.processes.expose = vec!["3000-3999".into()];
        base.limits.required = false;
        let one = resolve(&base, &[a.0.clone(), b.0.clone()], &[]).unwrap();
        let two = resolve(&base, &[b.0.clone(), a.0.clone(), a.0.clone()], &[]).unwrap();
        assert_eq!(one.fingerprint, two.fingerprint);
        assert_eq!(
            serde_json::to_value(&one.policy).unwrap(),
            serde_json::to_value(&two.policy).unwrap()
        );
        let limits = &one.policy.limits;
        assert_eq!(
            (limits.cpu_weight, limits.io_weight, limits.tasks_max),
            (10, 12, 100)
        );
        assert_eq!(limits.memory_high, (512u64 << 20).to_string());
        assert_eq!(limits.memory_max, (2u64 << 30).to_string());
        assert_eq!(
            (limits.idle_timeout_minutes, limits.agent_idle_minutes),
            (15, 2)
        );
        assert!(limits.required);
        assert_eq!(one.policy.processes.expose, vec!["3400-3500"]);
    }

    #[test]
    fn empty_exposure_means_no_auto_exposure_and_omitted_means_unchanged() {
        let repo = Repo::new();
        let mut base = Policy::builtin_default();
        base.processes.expose = vec!["3000-3999".into()];
        repo.write("[processes]\nexpose=[]");
        assert!(
            repo.resolve(&base)
                .unwrap()
                .policy
                .processes
                .expose
                .is_empty()
        );
        repo.write("[processes]");
        assert_eq!(
            repo.resolve(&base).unwrap().policy.processes.expose,
            base.processes.expose
        );
        base.processes.expose.clear();
        repo.write("[processes]\nexpose=['3000']");
        assert!(
            repo.resolve(&base)
                .unwrap()
                .policy
                .processes
                .expose
                .is_empty()
        );
    }

    #[test]
    fn finite_timeouts_override_unlimited_but_zero_never_widens() {
        let repo = Repo::new();
        let mut base = Policy::builtin_default();
        base.limits.idle_timeout_minutes = 0;
        repo.write("[limits]\nidle_timeout_minutes=20\nagent_idle_minutes=0");
        let out = repo.resolve(&base).unwrap();
        assert_eq!(out.policy.limits.idle_timeout_minutes, 20);
        assert_eq!(
            out.policy.limits.agent_idle_minutes,
            base.limits.agent_idle_minutes
        );
    }

    #[test]
    fn memory_units_are_exact_and_bad_caps_are_rejected() {
        assert_eq!(memory_bytes("1.5G").unwrap(), 1536 << 20);
        assert_eq!(memory_bytes("infinity").unwrap(), u64::MAX);
        for bad in [
            "NaN",
            "-1G",
            "inf",
            "0",
            "0G",
            "1.1",
            "18446744073709551615T",
            "1e5",
            "1%",
        ] {
            assert!(memory_bytes(bad).is_err(), "accepted {bad}");
        }
        let repo = Repo::new();
        repo.write("[limits]\nmemory_max='1G'");
        let result = repo.resolve(&Policy::builtin_default()).unwrap();
        assert_eq!(result.policy.limits.memory_high, (1u64 << 30).to_string());
        repo.write("[limits]\nmemory_max='infinity'");
        assert_eq!(
            repo.resolve(&Policy::builtin_default())
                .unwrap()
                .policy
                .limits
                .memory_max,
            (12u64 << 30).to_string()
        );
    }

    #[test]
    fn invalid_limits_and_ranges_are_rejected() {
        let repo = Repo::new();
        for text in [
            "[limits]\ncpu_weight=0",
            "[limits]\nio_weight=10001",
            "[limits]\ntasks_max=0",
            "[processes]\nexpose=['0']",
            "[processes]\nexpose=['4000-3000']",
            "[processes]\nexpose=['70000']",
            "[processes]\nexpose=['*']",
        ] {
            assert!(check_fragment(text, &repo.0).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn ignored_invalid_source_is_recorded_and_exception_is_not_sticky() {
        let repo = Repo::new();
        repo.write("[paths]\ndirect=['.']");
        let base = Policy::builtin_default();
        let error = format!("{:#}", repo.resolve(&base).unwrap_err());
        assert!(error.contains(&repo.0.join(FRAGMENT).display().to_string()));
        let out = resolve(
            &base,
            std::slice::from_ref(&repo.0),
            std::slice::from_ref(&repo.0),
        )
        .unwrap();
        assert!(out.sources[0].ignored);
        assert!(out.sources[0].text.contains("direct"));
        assert!(out.fragment_ro.is_empty());
        assert_eq!(
            out.fingerprint,
            input_fingerprint(&base, std::slice::from_ref(&repo.0)).unwrap()
        );
        assert!(repo.resolve(&base).is_err());
        let other = Repo::new();
        assert!(resolve(&base, &[repo.0.clone()], &[other.0.clone()]).is_err());
    }

    #[test]
    fn fingerprint_detects_invalid_edits_deletion_and_pin_changes() {
        let repo = Repo::new();
        let second = Repo::new();
        let base = Policy::builtin_default();
        repo.write("[paths]\nro=['src']");
        let before = repo.resolve(&base).unwrap();
        repo.write("invalid TOML [");
        let invalid = input_fingerprint(&base, std::slice::from_ref(&repo.0)).unwrap();
        assert_ne!(before.fingerprint, invalid);
        assert!(repo.resolve(&base).is_err());
        std::fs::remove_file(repo.0.join(FRAGMENT)).unwrap();
        let deleted = input_fingerprint(&base, std::slice::from_ref(&repo.0)).unwrap();
        assert_ne!(invalid, deleted);
        assert_ne!(
            deleted,
            input_fingerprint(&base, &[repo.0.clone(), second.0.clone()]).unwrap()
        );
        let mut changed_base = base.clone();
        changed_base.limits.tasks_max = 100;
        assert_ne!(
            deleted,
            input_fingerprint(&changed_base, std::slice::from_ref(&repo.0)).unwrap()
        );
        assert_eq!(before.sources[0].text, "[paths]\nro=['src']");
    }

    #[test]
    fn oversize_source_blocks_without_unbounded_read() {
        let repo = Repo::new();
        repo.write(&" ".repeat(MAX_FRAGMENT_BYTES + 1));
        assert!(repo.resolve(&Policy::builtin_default()).is_err());
    }

    #[test]
    fn file_pins_do_not_discover_parent_fragments() {
        let repo = Repo::new();
        repo.write("[invalid");
        let out = resolve(
            &Policy::builtin_default(),
            &[repo.0.join("src/lib.rs")],
            &[],
        )
        .unwrap();
        assert!(out.sources.is_empty());
    }
    #[test]
    fn non_utf8_pins_and_source_paths_survive_snapshot_roundtrip() {
        use std::os::unix::ffi::OsStringExt;
        let repo = Repo::new();
        let root = repo
            .0
            .join(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
        std::fs::create_dir_all(root.join(".sill")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let base = Policy::builtin_default();
        assert!(
            resolve(&base, &[root.clone()], &[])
                .unwrap()
                .sources
                .is_empty()
        );
        std::fs::write(
            root.join(FRAGMENT),
            "[paths]\nro=['src']\n[network]\nallow=['example.com']",
        )
        .unwrap();
        let resolved = resolve(&base, &[root.clone()], &[]).unwrap();
        let decoded: Resolution =
            serde_json::from_slice(&serde_json::to_vec(&resolved).unwrap()).unwrap();
        assert_eq!(decoded.sources[0].root, root);
        assert_eq!(decoded.sources[0].path, root.join(FRAGMENT));
        assert_eq!(decoded.suggestions[0].source, root.join(FRAGMENT));
        assert_eq!(decoded.fragment_ro, vec![root.join("src")]);
        assert_eq!(decoded.fingerprint, resolved.fingerprint);
    }
    #[test]
    fn repository_permissions_only_tighten_and_never_enable_backends() {
        let repo = Repo::new();
        let base = Policy::parse("name='development'\n[permissions]\npublic_web='on'").unwrap();
        repo.write("[permissions]\npublic_web='forbidden'\nhost_control='forbidden'");
        let resolved = repo.resolve(&base).unwrap();
        assert_eq!(resolved.policy.network.mode, NetMode::Allow);
        assert!(!resolved.policy.permissions.states(&resolved.policy.network, &[])[3].effective);
        assert_eq!(resolved.policy.permissions.host_control, crate::permissions::Rule::Forbidden);
        assert!(resolved.policy.permissions.validate_launch(&resolved.policy.network).is_ok());
        for text in ["[permissions]\ngpu='on'", "[permissions]\ngpu='off'", "[permissions]\nunknown='forbidden'"] {
            repo.write(text);
            assert!(repo.resolve(&base).is_err());
        }
    }

}
