//! The folders VS Code and the editors built from it have opened, for the
//! Import page and first-run setup to offer as projects.
//!
//! Each of these editors keeps a directory per opened folder under
//! `<user data>/User/workspaceStorage`, holding a `workspace.json` that
//! names the folder by URI, and the directory's last change says when the
//! folder was last used. Whether a folder is still there is the caller's to
//! ask: on macOS, looking at one in Documents or on the Desktop can put a
//! permission prompt in front of someone who has not asked to import.

use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::ui::icons::{BrandIcon, SvgIcon};
use crate::ui::draw::DrawContext;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use window::color::LinearRgba;

/// Editors known by their user-data directory, with what to call them.
/// Only these directories are looked in: on macOS, looking inside another
/// app's can ask the user for that app's data -- their contacts, say.
const KNOWN: &[(&str, &str, EditorKind)] = &[
    ("Code", "VS Code", EditorKind::VsCode),
    ("Code - Insiders", "VS Code Insiders", EditorKind::VsCode),
    ("Code - OSS", "Code - OSS", EditorKind::Other),
    ("VSCodium", "VSCodium", EditorKind::VsCodium),
    ("VSCodium - Insiders", "VSCodium Insiders", EditorKind::VsCodium),
    ("Cursor", "Cursor", EditorKind::Cursor),
    ("Windsurf", "Windsurf", EditorKind::Windsurf),
    ("Antigravity", "Antigravity", EditorKind::Other),
    ("Kiro", "Kiro", EditorKind::Other),
    ("Positron", "Positron", EditorKind::Other),
    ("Trae", "Trae", EditorKind::Other),
    ("Void", "Void", EditorKind::Other),
];

/// Opened folders read per editor, newest first: an install keeps one
/// directory per folder it ever opened, and only the recent ones matter.
const PER_EDITOR: usize = 400;

/// A `workspace.json` larger than this is not one; it is not read.
const MAX_WORKSPACE_JSON: u64 = 64 * 1024;

/// How recent a folder's last use has to be for its box to start ticked.
pub(crate) const RECENT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorKind {
    VsCode,
    VsCodium,
    Cursor,
    Windsurf,
    Other,
}

impl EditorKind {
    /// Its own mark, where it has one in colour.
    fn brand(self) -> Option<BrandIcon> {
        match self {
            Self::Cursor => Some(BrandIcon::Cursor),
            Self::Windsurf => Some(BrandIcon::Windsurf),
            Self::VsCodium => Some(BrandIcon::VSCodium),
            Self::VsCode | Self::Other => None,
        }
    }

    /// The glyph drawn white where it has no mark in colour.
    fn glyph(self) -> SvgIcon {
        match self {
            Self::VsCode => SvgIcon::VsCode,
            _ => SvgIcon::CodeXml,
        }
    }

    /// The colour of the tile its mark sits on.
    fn tile(self) -> LinearRgba {
        let (r, g, b) = match self {
            Self::VsCode => (0x00, 0x7A, 0xCC),
            Self::VsCodium => (0x1B, 0x2B, 0x44),
            Self::Cursor => (0x14, 0x14, 0x14),
            Self::Windsurf => (0x0B, 0x10, 0x0F),
            Self::Other => (0x48, 0x48, 0x4A),
        };
        LinearRgba::with_srgba(r, g, b, 0xFF)
    }
}

/// How far each tile in [`draw_stack`] starts after the one before, as a
/// share of its side.
const STACK_STEP: f32 = 0.7;

/// The kinds a stack shows: each once, in the order given, at most three.
pub(crate) fn stack_kinds(kinds: impl IntoIterator<Item = EditorKind>) -> Vec<EditorKind> {
    let mut stack = Vec::new();
    for kind in kinds {
        if stack.len() == 3 {
            break;
        }
        if !stack.contains(&kind) {
            stack.push(kind);
        }
    }
    stack
}

/// How wide [`draw_stack`] draws `count` tiles of side `tile`.
pub(crate) fn stack_width(count: usize, tile: f32) -> f32 {
    let tile = tile.round();
    tile + (tile * STACK_STEP).round() * count.saturating_sub(1) as f32
}

/// Editors' marks on tiles of side `tile`, from `x` rightwards, the first
/// in front and each ringed with `ground` -- what they sit on -- so the
/// ones behind show where they are cut. The tiles are lit as Settings
/// lights the squares heading its rows; `dark` is the chrome's appearance.
/// All on the top layer, where the marks are, so a tile in front hides the
/// mark behind it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_stack(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    kinds: &[EditorKind],
    x: f32,
    y: f32,
    tile: f32,
    ground: LinearRgba,
    dark: bool,
) -> anyhow::Result<()> {
    // Whole pixels, so the marks come out sharp.
    let (x, y, tile) = (x.round(), y.round(), tile.round());
    let step = (tile * STACK_STEP).round();
    let ring = (tile * 0.05).round().max(1.0);
    let radius = tile * 0.27;
    let mark = (tile * 0.56).round();
    for (index, kind) in kinds.iter().enumerate().rev() {
        let tile_x = x + step * index as f32;
        ctx.draw_rounded_rect(
            layers,
            2,
            tile_x - ring,
            y - ring,
            tile + ring * 2.0,
            tile + ring * 2.0,
            ground,
            radius + ring,
        )?;
        crate::ui::tile::draw_tile(
            ctx,
            layers,
            2,
            tile_x,
            y,
            tile,
            radius,
            kind.tile(),
            dark,
            &crate::settings_window::ROW_TILE,
        )?;
        let mark_x = tile_x + (tile - mark) / 2.0;
        let mark_y = y + (tile - mark) / 2.0;
        match kind.brand() {
            Some(brand) => ctx.draw_brand_icon(layers, brand, mark_x, mark_y, mark)?,
            None => ctx.draw_svg_icon(
                layers,
                kind.glyph(),
                mark_x,
                mark_y,
                mark,
                LinearRgba::with_srgba(0xFF, 0xFF, 0xFF, 0xFF),
            )?,
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Editor {
    pub name: String,
    pub kind: EditorKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Folder {
    pub path: PathBuf,
    pub name: String,
    /// Every editor that opened it, by index into `Scan::editors`, the most
    /// recent first.
    pub editors: Vec<usize>,
    pub last_used: SystemTime,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Scan {
    /// The editors with a folder here to offer, in the order found.
    pub editors: Vec<Editor>,
    /// One per folder, the most recently used first.
    pub folders: Vec<Folder>,
    /// Folders the editors opened on another machine -- over SSH, in WSL or
    /// a container -- which are not offered yet.
    pub remote: usize,
}

impl Scan {
    pub fn folders_in(&self, editor: Option<usize>) -> impl Iterator<Item = (usize, &Folder)> {
        self.folders
            .iter()
            .enumerate()
            .filter(move |(_, folder)| editor.map_or(true, |e| folder.editors.contains(&e)))
    }
}

/// The kinds of editor this machine has, each once, looked up the first
/// time they are asked for: the Import page offers its card only when
/// there is one. Nothing an editor opened is read for this.
pub(crate) fn installed() -> &'static [EditorKind] {
    static INSTALLED: std::sync::OnceLock<Vec<EditorKind>> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        let mut kinds = Vec::new();
        if let Some(root) = user_data_root() {
            for (_, kind, _) in editor_storages(&root) {
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
        }
        kinds
    })
}

/// What this machine's editors have opened, each folder as `resolve` names
/// it: two it names alike are one folder, and one it names None -- no
/// longer there, say -- is left out.
pub(crate) fn scan(resolve: &dyn Fn(&Path) -> Option<PathBuf>) -> Scan {
    match user_data_root() {
        Some(root) => scan_root(&root, resolve),
        None => Scan::default(),
    }
}

/// Where the editors keep their user data.
fn user_data_root() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(config::HOME_DIR.join("Library").join("Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| Some(config::HOME_DIR.join(".config")))
    }
}

/// One opened folder as an editor recorded it.
struct Opened {
    editor: usize,
    last_used: SystemTime,
    uri: String,
}

/// The editors under `root`, and what they opened, as for [`scan`].
fn scan_root(root: &Path, resolve: &dyn Fn(&Path) -> Option<PathBuf>) -> Scan {
    let mut editors = Vec::new();
    let mut opened = Vec::new();
    for (name, kind, storage) in editor_storages(root) {
        let index = editors.len();
        editors.push(Editor { name, kind });
        for (workspace_json, last_used) in newest_entries(&storage) {
            if let Some(uri) = read_folder_uri(&workspace_json) {
                opened.push(Opened {
                    editor: index,
                    last_used,
                    uri,
                });
            }
        }
    }
    merge(editors, opened, resolve)
}

/// Each known editor's `workspaceStorage` under `root`, where it has one.
fn editor_storages(root: &Path) -> Vec<(String, EditorKind, PathBuf)> {
    KNOWN
        .iter()
        .map(|(dir, name, kind)| {
            let storage = root.join(dir).join("User").join("workspaceStorage");
            (name.to_string(), *kind, storage)
        })
        .filter(|(.., storage)| storage.is_dir())
        .collect()
}

/// The `workspace.json` of the most recently used entries in `storage`,
/// with when each was last used: the later of its directory's change and
/// its state database's, which the editor writes as the folder is used.
fn newest_entries(storage: &Path) -> Vec<(PathBuf, SystemTime)> {
    let modified = |path: &Path| path.metadata().and_then(|m| m.modified()).ok();
    let mut entries: Vec<(PathBuf, SystemTime)> = std::fs::read_dir(storage)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let last_used = modified(&dir)?.max(
                modified(&dir.join("state.vscdb")).unwrap_or(SystemTime::UNIX_EPOCH),
            );
            Some((dir.join("workspace.json"), last_used))
        })
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    entries.truncate(PER_EDITOR);
    entries
}

/// The folder a `workspace.json` names, as the URI it gives. A multi-root
/// workspace names a `.code-workspace` file instead and is skipped.
fn read_folder_uri(workspace_json: &Path) -> Option<String> {
    if workspace_json.metadata().ok()?.len() > MAX_WORKSPACE_JSON {
        return None;
    }
    let text = std::fs::read(workspace_json).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&text).ok()?;
    value.get("folder")?.as_str().map(str::to_string)
}

/// One folder per path `resolve` names, the most recent use deciding its
/// time and the order its editors are listed in. Editors with nothing left
/// to offer are dropped, and the indexes renumbered.
fn merge(
    editors: Vec<Editor>,
    mut opened: Vec<Opened>,
    resolve: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Scan {
    opened.sort_by(|a, b| b.last_used.cmp(&a.last_used));
    let mut remote = HashSet::new();
    let mut folders: Vec<Folder> = Vec::new();
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    // Each path resolved once, however many editors opened it.
    let mut resolved: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    for item in opened {
        let Some(local) = local_path(&item.uri) else {
            // Opened on another machine: an SSH host, WSL, a container.
            if item.uri.contains("://") {
                remote.insert(item.uri);
            }
            continue;
        };
        let Some(path) = resolved
            .entry(local)
            .or_insert_with_key(|local| resolve(local))
            .clone()
        else {
            continue;
        };
        if let Some(&index) = by_path.get(&path) {
            let folder = &mut folders[index];
            if !folder.editors.contains(&item.editor) {
                folder.editors.push(item.editor);
            }
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        by_path.insert(path.clone(), folders.len());
        folders.push(Folder {
            path,
            name,
            editors: vec![item.editor],
            last_used: item.last_used,
        });
    }

    let mut renumber = HashMap::new();
    let mut kept = Vec::new();
    for (index, editor) in editors.into_iter().enumerate() {
        if folders.iter().any(|folder| folder.editors.contains(&index)) {
            renumber.insert(index, kept.len());
            kept.push(editor);
        }
    }
    for folder in &mut folders {
        for editor in &mut folder.editors {
            *editor = renumber[editor];
        }
    }
    Scan {
        editors: kept,
        folders,
        remote: remote.len(),
    }
}

/// A `file:` URI as the path it names on this machine. One naming another
/// machine -- `file://wsl.localhost/...` -- is not this machine's.
fn local_path(uri: &str) -> Option<PathBuf> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "file" || url.host_str().is_some_and(|host| !host.is_empty()) {
        return None;
    }
    url.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(days_ago: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(days_ago * 24 * 60 * 60)
    }

    fn opened(editor: usize, days_ago: u64, path: &Path) -> Opened {
        Opened {
            editor,
            last_used: at(days_ago),
            uri: url::Url::from_directory_path(path).unwrap().to_string(),
        }
    }

    fn editor(name: &str) -> Editor {
        Editor {
            name: name.to_string(),
            kind: EditorKind::Other,
        }
    }

    fn as_given(path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }

    #[test]
    fn a_folder_opened_in_two_editors_is_offered_once_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let api = root.path().join("api");
        let web = root.path().join("web");
        let scan = merge(
            vec![editor("VS Code"), editor("Cursor")],
            vec![
                opened(0, 9, &api),
                opened(1, 2, &api),
                opened(0, 1, &web),
            ],
            &as_given,
        );
        let paths: Vec<_> = scan.folders.iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths, [web, api]);
        // Cursor opened it last, so it is named first.
        assert_eq!(scan.folders[1].editors, [1, 0]);
        assert_eq!(scan.folders[1].name, "api");
        assert_eq!(scan.folders_in(Some(1)).count(), 1);
        assert_eq!(scan.folders_in(None).count(), 2);
    }

    #[test]
    fn removed_and_remote_folders_are_not_offered() {
        let root = tempfile::tempdir().unwrap();
        let gone = root.path().join("gone");
        let here = root.path().join("here");
        let scan = merge(
            vec![editor("VS Code"), editor("Windsurf")],
            vec![
                opened(0, 1, &gone),
                opened(0, 2, &here),
                Opened {
                    editor: 1,
                    last_used: at(3),
                    uri: "vscode-remote://ssh-remote%2Bserver-a/home/user/app".into(),
                },
                Opened {
                    editor: 1,
                    last_used: at(4),
                    uri: "vscode-remote://ssh-remote%2Bserver-a/home/user/app".into(),
                },
            ],
            &|path| (path != gone).then(|| path.to_path_buf()),
        );
        assert_eq!(scan.folders.len(), 1);
        assert_eq!(scan.remote, 1, "the same remote folder counts once");
        // Windsurf had only the remote folder: no pill for it.
        assert_eq!(scan.editors, [editor("VS Code")]);
        assert_eq!(scan.folders[0].editors, [0]);
    }

    #[test]
    fn editors_are_found_by_their_storage_and_read_from_workspace_json() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let write = |dir: &str, entry: &str, json: String| {
            let storage = root.path().join(dir).join("User").join("workspaceStorage").join(entry);
            std::fs::create_dir_all(&storage).unwrap();
            std::fs::write(storage.join("workspace.json"), json).unwrap();
        };
        let uri = url::Url::from_directory_path(project.path()).unwrap();
        write("Cursor", "a", format!(r#"{{"folder":"{uri}"}}"#));
        write("Trae", "b", format!(r#"{{"folder":"{uri}"}}"#));
        write("Code", "c", r#"{"workspace":"file:///code/team.code-workspace"}"#.into());
        // Laid out like an editor but not a known one: not looked in.
        write("Some Fork", "d", format!(r#"{{"folder":"{uri}"}}"#));

        let scan = scan_root(root.path(), &|path: &Path| {
            path.is_dir().then(|| path.to_path_buf())
        });
        let names: Vec<_> = scan.editors.iter().map(|e| e.name.as_str()).collect();
        // VS Code opened only a multi-root workspace, which is skipped.
        assert_eq!(names, ["Cursor", "Trae"]);
        assert_eq!(scan.editors[0].kind, EditorKind::Cursor);
        assert_eq!(scan.folders.len(), 1);
        assert_eq!(scan.folders[0].editors.len(), 2);
    }

    #[test]
    fn two_spellings_of_one_folder_are_offered_once() {
        let root = tempfile::tempdir().unwrap();
        let alias = root.path().join("alias");
        let canonical = root.path().join("canonical");
        let resolve = |path: &Path| {
            Some(match path.strip_prefix(&alias) {
                Ok(rest) => canonical.join(rest),
                Err(_) => path.to_path_buf(),
            })
        };
        let scan = merge(
            vec![editor("VS Code"), editor("Cursor")],
            vec![
                opened(0, 1, &alias.join("demo")),
                opened(1, 2, &canonical.join("demo")),
            ],
            &resolve,
        );
        assert_eq!(scan.folders.len(), 1);
        assert_eq!(scan.folders[0].path, canonical.join("demo"));
        assert_eq!(scan.folders[0].editors, [0, 1]);
    }

    #[test]
    fn a_file_uri_naming_another_machine_is_not_a_local_folder() {
        let scan = merge(
            vec![editor("VS Code")],
            vec![Opened {
                editor: 0,
                last_used: at(1),
                uri: "file://wsl.localhost/Alpine/home/user/app".into(),
            }],
            &as_given,
        );
        assert!(scan.folders.is_empty());
        assert_eq!(scan.remote, 1);
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_drive_is_read_with_its_colon_escaped() {
        // VS Code writes the drive's colon as %3A.
        assert_eq!(
            local_path("file:///c%3A/Users/user/app"),
            Some(PathBuf::from(r"c:\Users\user\app"))
        );
    }
}
