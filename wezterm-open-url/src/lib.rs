// Portions of this file are derived from code that is
// Copyright © 2015 Sebastian Thiel
// <https://github.com/Byron/open-rs>

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenWithCandidate {
    pub id: String,
    pub label: String,
    pub icon_path: Option<PathBuf>,
    pub is_default: bool,
}

pub fn open_path_with_candidate(path: &Path, candidate_id: &str) {
    open_with(&path.to_string_lossy(), candidate_id);
}

pub fn open_with_candidates(path: &Path) -> Vec<OpenWithCandidate> {
    platform_open_with_candidates(path)
}

/// Normalize a path chosen in the OS app picker into an open-with candidate
/// (id, label). Linux `.desktop` entries become `desktop:{id}` with the
/// label taken from their `Name=`; everything else (mac .app bundles,
/// executables) uses the path as id and the file stem as label.
pub fn app_candidate_for_picked_path(path: &Path) -> (String, String) {
    #[cfg(all(not(windows), not(target_os = "macos")))]
    if path
        .extension()
        .is_some_and(|extension| extension == "desktop")
    {
        let id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let label = desktop_name(path).unwrap_or_else(|| id.clone());
        return (format!("desktop:{id}"), label);
    }

    let label = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    (path.to_string_lossy().into_owned(), label)
}

#[cfg(target_os = "macos")]
fn platform_open_with_candidates(path: &Path) -> Vec<OpenWithCandidate> {
    macos_open_with_candidates(path)
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn platform_open_with_candidates(path: &Path) -> Vec<OpenWithCandidate> {
    linux_open_with_candidates(path)
}

#[cfg(windows)]
fn platform_open_with_candidates(path: &Path) -> Vec<OpenWithCandidate> {
    windows_open_with_candidates(path)
}

/// Discover candidate applications for a file on Windows: the association
/// default (AssocQueryString) plus Explorer's per-user OpenWithList MRU and
/// the OpenWithProgids registered for the extension.
#[cfg(windows)]
fn windows_open_with_candidates(path: &Path) -> Vec<OpenWithCandidate> {
    use std::collections::HashSet;
    use winreg::enums::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER};
    use winreg::RegKey;

    let Some(ext) = path.extension() else {
        return Vec::new();
    };
    let ext = format!(".{}", ext.to_string_lossy().to_lowercase());

    let mut out: Vec<OpenWithCandidate> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    fn push_candidate(
        out: &mut Vec<OpenWithCandidate>,
        seen: &mut HashSet<String>,
        id: String,
        is_default: bool,
    ) {
        let key = id.to_lowercase();
        if id.is_empty() || seen.contains(&key) {
            return;
        }
        seen.insert(key);
        let label = Path::new(&id)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| id.clone());
        out.push(OpenWithCandidate {
            id,
            label,
            icon_path: None,
            is_default,
        });
    }

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let hkcr = RegKey::predef(HKEY_CLASSES_ROOT);
    let file_exts = format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext}");

    if let Some(exe) = windows_default_executable(&hkcu, &hkcr, &file_exts, &ext) {
        push_candidate(&mut out, &mut seen, exe, true);
    }

    // Explorer's MRU of "Open with" choices; values are exe names that
    // ShellExecuteW resolves via PATH / App Paths.
    if let Ok(key) = hkcu.open_subkey(format!(r"{file_exts}\OpenWithList")) {
        for (name, value) in key.enum_values().flatten() {
            if name.eq_ignore_ascii_case("MRUList") {
                continue;
            }
            if let Ok(exe) = <String as winreg::types::FromRegValue>::from_reg_value(&value) {
                if exe.to_lowercase().ends_with(".exe") {
                    push_candidate(&mut out, &mut seen, exe, false);
                }
            }
        }
    }

    // ProgIds registered for the extension (per-user and machine-wide),
    // plus the extension's default progid.
    let mut progids: Vec<String> = Vec::new();
    if let Ok(key) = hkcu.open_subkey(format!(r"{file_exts}\OpenWithProgids")) {
        progids.extend(key.enum_values().flatten().map(|(name, _)| name));
    }
    if let Ok(key) = hkcr.open_subkey(format!(r"{ext}\OpenWithProgids")) {
        progids.extend(key.enum_values().flatten().map(|(name, _)| name));
    }
    if let Ok(key) = hkcr.open_subkey(&ext) {
        if let Ok(progid) = key.get_value::<String, _>("") {
            progids.push(progid);
        }
    }
    for progid in progids {
        if let Some(exe) = windows_progid_executable(&hkcr, &progid) {
            push_candidate(&mut out, &mut seen, exe, false);
        }
    }

    out.truncate(20);
    out
}

/// Resolve a progid to the executable of its `shell\open\command`.
#[cfg(windows)]
fn windows_progid_executable(hkcr: &winreg::RegKey, progid: &str) -> Option<String> {
    if progid.is_empty() {
        return None;
    }
    let key = hkcr
        .open_subkey(format!(r"{progid}\shell\open\command"))
        .ok()?;
    let command: String = key.get_value("").ok()?;
    windows_command_line_executable(&command)
}

/// Extract the executable path from a registry `shell\open\command` value.
#[cfg(windows)]
fn windows_command_line_executable(command: &str) -> Option<String> {
    let command = command.trim();
    let exe = if let Some(rest) = command.strip_prefix('"') {
        rest.split('"').next()?
    } else {
        command.split_whitespace().next()?
    };
    let exe = exe.trim();
    if exe.is_empty() || !exe.to_lowercase().ends_with(".exe") {
        return None;
    }
    Some(exe.to_string())
}

/// The user's default handler for an extension: Explorer's UserChoice progid
/// first (what modern Windows actually honours), falling back to the
/// extension's classic default progid under HKCR.
#[cfg(windows)]
fn windows_default_executable(
    hkcu: &winreg::RegKey,
    hkcr: &winreg::RegKey,
    file_exts: &str,
    ext: &str,
) -> Option<String> {
    if let Ok(key) = hkcu.open_subkey(format!(r"{file_exts}\UserChoice")) {
        if let Ok(progid) = key.get_value::<String, _>("ProgId") {
            if let Some(exe) = windows_progid_executable(hkcr, &progid) {
                return Some(exe);
            }
        }
    }
    let key = hkcr.open_subkey(ext).ok()?;
    let progid: String = key.get_value("").ok()?;
    windows_progid_executable(hkcr, &progid)
}

#[cfg(not(windows))]
pub fn open_url(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let candidates: &[&[&str]] = &[&["/usr/bin/open", &url]];

        #[cfg(not(target_os = "macos"))]
        let candidates: &[&[&str]] = &[
            &["xdg-open", &url],
            &["gio", "open", &url] as &[_],
            &["gnome-open", &url],
            &["kde-open", &url],
            &["wslview", &url],
        ];

        for candidate in candidates {
            let mut cmd = std::process::Command::new(candidate[0]);
            cmd.args(&candidate[1..]);

            if let Ok(status) = cmd.status() {
                if status.success() {
                    return;
                }
            }
        }
    });
}

#[cfg(not(windows))]
pub fn open_with(url: &str, app: &str) {
    let url = url.to_string();
    let app = app.to_string();

    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let args: &[&str] = &["/usr/bin/open", "-a", &app, &url];

        #[cfg(not(target_os = "macos"))]
        let mut cmd = if let Some(desktop_id) = app.strip_prefix("desktop:") {
            let mut cmd = std::process::Command::new("gtk-launch");
            cmd.arg(desktop_id).arg(&url);
            cmd
        } else {
            let mut cmd = std::process::Command::new(&app);
            cmd.arg(&url);
            cmd
        };

        #[cfg(target_os = "macos")]
        let mut cmd = {
            let mut cmd = std::process::Command::new(args[0]);
            cmd.args(&args[1..]);
            cmd
        };

        if let Ok(status) = cmd.status() {
            if status.success() {
                return;
            }
        }
    });
}

#[cfg(target_os = "macos")]
pub fn reveal_path(path: &std::path::Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(path)
            .status();
    });
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn reveal_path(path: &std::path::Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        for candidate in ["xdg-open", "gio", "gnome-open", "kde-open", "wslview"] {
            let mut cmd = std::process::Command::new(candidate);
            if candidate == "gio" {
                cmd.arg("open");
            }
            cmd.arg(&path);

            if let Ok(status) = cmd.status() {
                if status.success() {
                    return;
                }
            }
        }
    });
}

#[cfg(windows)]
fn shell_execute(url: String, with: Option<String>) {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::shellapi::ShellExecuteW;
    /// Convert a rust string to a windows wide string
    fn wide_string(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    std::thread::spawn(move || {
        let operation = wide_string("open");

        let url = wide_string(&url);
        let with = with.map(|s| wide_string(&s));

        let (app, path) = match with {
            Some(app) => (app.as_ptr(), url.as_ptr()),
            None => (url.as_ptr(), std::ptr::null()),
        };

        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                app,
                path,
                std::ptr::null(),
                winapi::um::winuser::SW_SHOW,
            );
        }
    });
}

#[cfg(windows)]
pub fn open_url(url: &str) {
    shell_execute(url.to_string(), None);
}

#[cfg(windows)]
pub fn open_with(url: &str, app: &str) {
    shell_execute(url.to_string(), Some(app.to_string()));
}

/// Show `path` in Explorer with the item selected.
///
/// Deliberately not `shell_execute`: that is the same `ShellExecuteW("open")`
/// that `open_url` uses, so revealing a `.exe`, `.bat` or `.ps1` ran it.
#[cfg(windows)]
pub fn reveal_path(path: &std::path::Path) {
    use std::os::windows::process::CommandExt as _;

    let path = path.to_path_buf();
    std::thread::spawn(move || {
        // `canonicalize` fails when the item is gone; open the folder that
        // would have held it rather than nothing. Never the item itself.
        let Ok(full) = std::fs::canonicalize(&path) else {
            if let Some(parent) = path.parent() {
                let _ = std::process::Command::new("explorer.exe").arg(parent).spawn();
            }
            return;
        };
        // Explorer parses its own command line and wants `/select,` and the
        // path as one argument; the usual quoting splits that into two paths
        // as soon as the path has a space, so it is passed verbatim.
        //
        // Its exit status says nothing -- Explorer returns 1 even when it
        // worked -- so nothing here may branch on it. In particular there is
        // no falling back to `shell_execute`, which is what ran the file.
        let _ = std::process::Command::new("explorer.exe")
            .raw_arg(select_argument(&full.to_string_lossy()))
            .spawn();
    });
}

/// The `/select,` argument for a full path.
///
/// `canonicalize` hands back a verbatim path, which Explorer does not
/// understand in either of its forms: `\\?\C:\x` has to lose the prefix, and
/// `\\?\UNC\server\share` is spelled `\\server\share` everywhere else.
#[cfg(windows)]
fn select_argument(full: &str) -> String {
    let full = match full.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => full.strip_prefix(r"\\?\").unwrap_or(full).to_string(),
    };
    format!("/select,\"{full}\"")
}

#[cfg(all(test, windows))]
mod tests {
    /// One argument, quoted, and an ordinary path inside it. Explorer reads
    /// its own command line: unquoted, a path with a space becomes two
    /// paths, and a verbatim prefix is not a path it knows at all.
    #[test]
    fn the_select_argument_is_one_quoted_ordinary_path() {
        assert_eq!(
            super::select_argument(r"\\?\C:\Users\ada\Q3 plan.bat"),
            r#"/select,"C:\Users\ada\Q3 plan.bat""#
        );
        // A UNC path keeps its UNC spelling, not the verbatim one.
        assert_eq!(
            super::select_argument(r"\\?\UNC\server\share\f.txt"),
            r#"/select,"\\server\share\f.txt""#
        );
        // Never canonicalized, so never prefixed: unchanged.
        assert_eq!(super::select_argument(r"C:\x.txt"), r#"/select,"C:\x.txt""#);
    }
}

#[cfg(target_os = "macos")]
fn macos_open_with_candidates(path: &Path) -> Vec<OpenWithCandidate> {
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::TCFType;
    use core_foundation::bundle::CFBundle;
    use core_foundation::string::{CFString, CFStringRef};
    use core_foundation::url::{kCFURLPOSIXPathStyle, CFURLRef, CFURL};
    use core_foundation_sys::base::CFTypeRef;
    use core_foundation_sys::error::CFErrorRef;

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSCopyApplicationURLsForURL(url: CFURLRef, roles: u32) -> CFArrayRef;
        fn LSCopyDefaultApplicationURLForURL(
            url: CFURLRef,
            roles: u32,
            out_error: *mut CFErrorRef,
        ) -> CFURLRef;
    }

    const K_LS_ROLES_ALL: u32 = 0xffff_ffff;

    fn app_display_name(app_url: &CFURL) -> Option<String> {
        let bundle = CFBundle::new(app_url.clone())?;
        let info = bundle.info_dictionary();
        for key in ["CFBundleDisplayName", "CFBundleName"] {
            let key = CFString::new(key);
            if let Some(value) = info.find(&key) {
                let name =
                    unsafe { CFString::wrap_under_get_rule(value.as_CFTypeRef() as CFStringRef) };
                let name = name.to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
        None
    }

    let Some(file_url) = CFURL::from_path(path, path.is_dir()) else {
        return Vec::new();
    };

    let default_app_path = unsafe {
        let default_ref = LSCopyDefaultApplicationURLForURL(
            file_url.as_concrete_TypeRef(),
            K_LS_ROLES_ALL,
            std::ptr::null_mut(),
        );
        (!default_ref.is_null()).then(|| {
            CFURL::wrap_under_create_rule(default_ref)
                .get_file_system_path(kCFURLPOSIXPathStyle)
                .to_string()
        })
    };

    let mut candidates: Vec<OpenWithCandidate> = Vec::new();
    let array_ref =
        unsafe { LSCopyApplicationURLsForURL(file_url.as_concrete_TypeRef(), K_LS_ROLES_ALL) };
    if !array_ref.is_null() {
        let array: CFArray<CFTypeRef> = unsafe { TCFType::wrap_under_create_rule(array_ref) };
        for value in array.get_all_values() {
            if value.is_null() {
                continue;
            }

            let app_url = unsafe { CFURL::wrap_under_get_rule(value as CFURLRef) };
            let app_path = app_url
                .get_file_system_path(kCFURLPOSIXPathStyle)
                .to_string();
            if app_path.is_empty() || candidates.iter().any(|candidate| candidate.id == app_path) {
                continue;
            }

            let label = app_display_name(&app_url)
                .or_else(|| {
                    Path::new(&app_path)
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| app_path.clone());
            candidates.push(OpenWithCandidate {
                is_default: default_app_path.as_deref() == Some(app_path.as_str()),
                id: app_path,
                label,
                icon_path: None,
            });
        }
    }

    if let Some(default_app_path) = default_app_path {
        if !candidates
            .iter()
            .any(|candidate| candidate.id == default_app_path)
        {
            let label = Path::new(&default_app_path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_else(|| default_app_path.clone());
            candidates.push(OpenWithCandidate {
                is_default: true,
                id: default_app_path,
                label,
                icon_path: None,
            });
        }
    }

    candidates.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    candidates.truncate(20);
    candidates
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn linux_open_with_candidates(path: &Path) -> Vec<OpenWithCandidate> {
    let Some(mime) = xdg_mime_type(path) else {
        return Vec::new();
    };

    let default_desktop_id = xdg_default_desktop_id(&mime);
    let mut desktop_ids = Vec::new();
    if let Some(default_id) = default_desktop_id.clone() {
        desktop_ids.push(default_id);
    }
    desktop_ids.extend(xdg_associated_desktop_ids(&mime));
    desktop_ids.sort();
    desktop_ids.dedup();
    if let Some(default_id) = default_desktop_id.as_deref() {
        desktop_ids.sort_by(|a, b| {
            (a.as_str() != default_id)
                .cmp(&(b.as_str() != default_id))
                .then_with(|| a.cmp(b))
        });
    }

    desktop_ids
        .into_iter()
        .take(20)
        .filter_map(|desktop_id| {
            let desktop = find_desktop_file(&desktop_id)?;
            let label = desktop_name(&desktop).unwrap_or_else(|| desktop_id.clone());
            Some(OpenWithCandidate {
                is_default: default_desktop_id.as_deref() == Some(desktop_id.as_str()),
                id: format!("desktop:{desktop_id}"),
                label,
                icon_path: None,
            })
        })
        .collect()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn xdg_mime_type(path: &Path) -> Option<String> {
    let output = std::process::Command::new("xdg-mime")
        .arg("query")
        .arg("filetype")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mime = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!mime.is_empty()).then_some(mime)
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn xdg_default_desktop_id(mime: &str) -> Option<String> {
    let output = std::process::Command::new("xdg-mime")
        .arg("query")
        .arg("default")
        .arg(mime)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let desktop_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!desktop_id.is_empty()).then_some(desktop_id)
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn xdg_associated_desktop_ids(mime: &str) -> Vec<String> {
    xdg_mimeapps_files()
        .into_iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|contents| desktop_ids_for_mimeapps(&contents, mime))
        .collect()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn desktop_ids_for_mimeapps(contents: &str, mime: &str) -> Vec<String> {
    let mut in_interesting_section = false;
    let mut ids = Vec::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_interesting_section = matches!(
                line,
                "[Default Applications]" | "[Added Associations]" | "[MIME Cache]"
            );
            continue;
        }
        if !in_interesting_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key != mime {
            continue;
        }
        ids.extend(
            value
                .split(';')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
        );
    }
    ids
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn xdg_mimeapps_files() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(config_home).join("mimeapps.list"));
    } else if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".config/mimeapps.list"));
    }
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        paths.push(PathBuf::from(data_home).join("applications/mimeapps.list"));
    } else if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".local/share/applications/mimeapps.list"));
    }
    paths.extend(
        std::env::var_os("XDG_DATA_DIRS")
            .map(|value| {
                std::env::split_paths(&value)
                    .map(|path| path.join("applications/mimeapps.list"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                vec![
                    PathBuf::from("/usr/local/share/applications/mimeapps.list"),
                    PathBuf::from("/usr/share/applications/mimeapps.list"),
                ]
            }),
    );
    paths
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn find_desktop_file(desktop_id: &str) -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        roots.push(PathBuf::from(data_home).join("applications"));
    } else if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".local/share/applications"));
    }
    if let Some(data_dirs) = std::env::var_os("XDG_DATA_DIRS") {
        roots.extend(std::env::split_paths(&data_dirs).map(|path| path.join("applications")));
    } else {
        roots.push(PathBuf::from("/usr/local/share/applications"));
        roots.push(PathBuf::from("/usr/share/applications"));
    }

    for root in roots {
        let direct = root.join(desktop_id);
        if direct.exists() {
            return Some(direct);
        }
        let nested = root.join(desktop_id.replace('-', "/"));
        if nested.exists() {
            return Some(nested);
        }
    }
    None
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn desktop_name(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    contents.lines().find_map(|raw_line| {
        let line = raw_line.trim();
        line.strip_prefix("Name=")
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    })
}
