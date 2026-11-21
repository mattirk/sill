//! A path of this machine as a path of the box, for a folder the person picked here. Where the
//! window's system has Unix paths the two are the same bytes. On Windows a folder shared with
//! Silli's VM is there under `/mnt/<drive letter>/…` (docs/silli-windows.md, section 7.2).
use silli_proto::sessions::HostPath;

/// The box's path of `local`; `None` for a path that has none there (on Windows: not on a
/// lettered drive, or not absolute).
pub fn from_local(local: &std::path::Path) -> Option<HostPath> {
    #[cfg(unix)]
    return Some(HostPath::from_path(local));
    #[cfg(not(unix))]
    windows_to_box(&local.to_string_lossy()).map(|path| HostPath::from_bytes(path.as_bytes()))
}

/// `C:\Users\me\project` as `/mnt/c/Users/me/project`: the drive letter in lower case under
/// `/mnt`, as WSL writes it. Only a path on a lettered drive has one: a network path
/// (`\\server\share`) does not.
#[cfg_attr(unix, allow(dead_code))]
pub fn windows_to_box(path: &str) -> Option<String> {
    // the long-path form of the same thing
    let path = path.strip_prefix(r"\\?\").filter(|rest| !rest.starts_with("UNC")).unwrap_or(path);
    let mut chars = path.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    if chars.next() != Some(':') {
        return None;
    }
    let rest = chars.as_str();
    if !(rest.is_empty() || rest.starts_with(['\\', '/'])) {
        return None;
    }
    let mut box_path = format!("/mnt/{}", drive.to_ascii_lowercase());
    for part in rest.split(['\\', '/']).filter(|part| !part.is_empty()) {
        if part == "." || part == ".." {
            return None;
        }
        box_path.push('/');
        box_path.push_str(part);
    }
    Some(box_path)
}

#[cfg(test)]
mod tests {
    use super::windows_to_box;

    #[test]
    fn a_windows_folder_has_one_place_in_the_box() {
        assert_eq!(windows_to_box(r"C:\Users\me\Documents\project").as_deref(), Some("/mnt/c/Users/me/Documents/project"));
        assert_eq!(windows_to_box(r"D:\work\").as_deref(), Some("/mnt/d/work"));
        assert_eq!(windows_to_box("c:/Users/me").as_deref(), Some("/mnt/c/Users/me"));
        assert_eq!(windows_to_box(r"\\?\C:\Users\me").as_deref(), Some("/mnt/c/Users/me"), "the long-path form");
        assert_eq!(windows_to_box(r"E:\").as_deref(), Some("/mnt/e"));
        for none in [r"\\server\share\x", r"\\?\UNC\server\share", r"relative\path", r"C:relative", "/home/me", r"C:\a\..\b", ""] {
            assert_eq!(windows_to_box(none), None, "{none}");
        }
    }
}
