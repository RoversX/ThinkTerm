use super::MarkdownDocumentSession;
use anyhow::{bail, ensure, Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Weak};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SaveDocumentOutcome {
    pub saved_revision: u64,
    pub current_revision: u64,
}

const MAX_IMPORTED_IMAGE_BYTES: u64 = 25 * 1024 * 1024;
const SUPPORTED_IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff",
];

#[derive(Clone)]
pub(crate) struct VaultDocument {
    pub vault_root: PathBuf,
    pub relative_path: String,
    pub document_path: PathBuf,
    pub session: Arc<Mutex<MarkdownDocumentSession>>,
}

lazy_static::lazy_static! {
    /// A document opened in two ThinkTerm windows must share one session, or
    /// two independent autosave loops can silently overwrite each other. Weak
    /// values let closed documents fall out without a separate eviction pass.
    static ref DOCUMENT_SESSIONS: Mutex<HashMap<(PathBuf, String), Weak<Mutex<MarkdownDocumentSession>>>> =
        Mutex::new(HashMap::new());
    static ref ATTACHMENT_IMPORT_LOCK: Mutex<()> = Mutex::new(());
}

pub(crate) fn open_vault_document(
    vault_root: &Path,
    relative_path: &str,
    create: bool,
) -> Result<VaultDocument> {
    ensure!(
        vault_root.is_dir(),
        "Vault is not a directory: {}",
        vault_root.display()
    );
    let vault_root = vault_root
        .canonicalize()
        .with_context(|| format!("resolve Vault {}", vault_root.display()))?;
    let relative_path = crate::workspace_threads::normalize_vault_markdown_path(relative_path)?;
    let document_path = safe_content_path(&vault_root, Path::new(&relative_path))?;
    if !document_path.exists() {
        ensure!(create, "note does not exist: {relative_path}");
        write_bytes_atomic(&document_path, b"")?;
    }
    ensure!(
        document_path.is_file(),
        "note is not a file: {}",
        document_path.display()
    );

    let key = (vault_root.clone(), relative_path.clone());
    let existing = {
        let mut sessions = DOCUMENT_SESSIONS.lock();
        sessions.retain(|_, session| session.strong_count() > 0);
        sessions.get(&key).and_then(Weak::upgrade)
    };
    let session = if let Some(session) = existing {
        session
    } else {
        // Reading a multi-megabyte Note and constructing its line index must not
        // hold the process-wide document registry lock.  Two windows may race
        // here; the second registry check below selects one canonical session
        // and lets the unused candidate fall out normally.
        let source = fs::read_to_string(&document_path)
            .with_context(|| format!("read {}", document_path.display()))?;
        let candidate = Arc::new(Mutex::new(MarkdownDocumentSession::new(
            format!("{}::{relative_path}", vault_root.display()),
            document_path.clone(),
            source,
        )));
        let mut sessions = DOCUMENT_SESSIONS.lock();
        sessions.retain(|_, session| session.strong_count() > 0);
        if let Some(session) = sessions.get(&key).and_then(Weak::upgrade) {
            session
        } else {
            sessions.insert(key, Arc::downgrade(&candidate));
            candidate
        }
    };

    Ok(VaultDocument {
        vault_root,
        relative_path,
        document_path,
        session,
    })
}

pub(crate) fn vault_markdown_paths(vault_root: &Path) -> Result<Vec<String>> {
    Ok(vault_file_paths(vault_root)?
        .into_iter()
        .filter(|path| {
            Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect())
}

pub(crate) fn vault_file_paths(vault_root: &Path) -> Result<Vec<String>> {
    let canonical_root = vault_root
        .canonicalize()
        .with_context(|| format!("resolve Vault {}", vault_root.display()))?;
    let mut paths = walkdir::WalkDir::new(&canonical_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || entry
                    .file_name()
                    .to_str()
                    .is_none_or(|name| !name.starts_with('.'))
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(&canonical_root)
                .ok()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
        })
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| path.to_ascii_lowercase());
    Ok(paths)
}

pub(crate) fn save_document_revision(
    session: &Arc<Mutex<MarkdownDocumentSession>>,
) -> Result<SaveDocumentOutcome> {
    let save_lock = session.lock().save_lock();
    let _save_guard = save_lock.lock();
    let (revision, path, source) = session.lock().snapshot_for_save();
    let result = write_bytes_atomic(&path, source.as_bytes());
    let finish_result = result
        .as_ref()
        .map(|_| ())
        .map_err(|err| anyhow::anyhow!("{err:#}"));
    let current_revision = {
        let mut session = session.lock();
        session.finish_save(revision, source, finish_result);
        session.revision()
    };
    result.map(|_| SaveDocumentOutcome {
        saved_revision: revision,
        current_revision,
    })
}

pub(crate) fn import_attachment(document: &VaultDocument, source: &Path) -> Result<String> {
    let _operation_guard = ATTACHMENT_IMPORT_LOCK.lock();
    let metadata = fs::metadata(source).with_context(|| format!("stat {}", source.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a file", source.display());
    }
    if metadata.len() > MAX_IMPORTED_IMAGE_BYTES {
        bail!("image exceeds the 25 MiB attachment limit");
    }
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .context("image has no extension")?;
    if !SUPPORTED_IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        bail!("unsupported image type: {extension}");
    }

    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .map(sanitize_file_stem)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "image".to_string());
    let document_parent = document
        .document_path
        .parent()
        .context("Note document has no parent")?;
    let attachment_dir = document_parent.join("attachments");
    fs::create_dir_all(&attachment_dir)
        .with_context(|| format!("create {}", attachment_dir.display()))?;
    ensure_canonical_descendant(&document.vault_root, &attachment_dir)?;
    let mut sequence = 1usize;
    let destination = loop {
        let name = if sequence == 1 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem}-{sequence}.{extension}")
        };
        let candidate = attachment_dir.join(name);
        if !candidate.exists() {
            break candidate;
        }
        sequence += 1;
    };

    let mut input = fs::File::open(source).with_context(|| format!("open {}", source.display()))?;
    let mut temporary = NamedTempFile::new_in(&attachment_dir).with_context(|| {
        format!(
            "create temporary attachment in {}",
            attachment_dir.display()
        )
    })?;
    std::io::copy(&mut input, &mut temporary)
        .with_context(|| format!("copy {}", source.display()))?;
    temporary.flush().context("flush attachment")?;
    temporary.as_file().sync_all().context("sync attachment")?;
    temporary
        .persist(&destination)
        .with_context(|| format!("persist {}", destination.display()))?;
    set_user_document_permissions(&destination, None);

    let relative = destination
        .strip_prefix(document_parent)
        .context("attachment escaped Note directory")?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn resolve_local_image(
    content_root: &Path,
    document_path: &Path,
    target: &str,
) -> Result<PathBuf> {
    if target.starts_with("http://") || target.starts_with("https://") {
        bail!("network images are not loaded");
    }
    let target = target.split(['?', '#']).next().unwrap_or(target);
    let relative = Path::new(target);
    if relative.is_absolute() {
        bail!("absolute image paths are not allowed");
    }
    let parent = document_path.parent().context("document has no parent")?;
    let candidate = parent.join(relative);
    let canonical_root = content_root
        .canonicalize()
        .with_context(|| format!("resolve {}", content_root.display()))?;
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolve {}", candidate.display()))?;
    if !canonical.starts_with(&canonical_root) {
        bail!("image path escapes Notebook content");
    }
    Ok(canonical)
}

fn safe_content_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("invalid Vault document path {}", relative.display());
    }
    let candidate = root.join(relative);
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("resolve {}", root.display()))?;
    let mut existing = candidate.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .context("Vault document path has no existing parent")?;
    }
    let canonical_existing = existing
        .canonicalize()
        .with_context(|| format!("resolve {}", existing.display()))?;
    if !canonical_existing.starts_with(&canonical_root) {
        bail!("Vault document path escapes root");
    }
    Ok(candidate)
}

fn ensure_canonical_descendant(root: &Path, path: &Path) -> Result<()> {
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("resolve {}", root.display()))?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    if !canonical_path.starts_with(canonical_root) {
        bail!("{} escapes Vault root", path.display());
    }
    Ok(())
}

fn sanitize_file_stem(stem: &str) -> String {
    let mut result = String::new();
    let mut previous_dash = false;
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            result.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            result.push('-');
            previous_dash = true;
        }
    }
    result.trim_matches('-').chars().take(80).collect()
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Note path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let existing_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("write {}", path.display()))?;
    temporary
        .flush()
        .with_context(|| format!("flush {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("sync {}", path.display()))?;
    temporary
        .persist(path)
        .with_context(|| format!("replace {}", path.display()))?;
    set_user_document_permissions(path, existing_permissions);
    Ok(())
}

#[cfg(unix)]
fn set_user_document_permissions(path: &Path, permissions: Option<fs::Permissions>) {
    use std::os::unix::fs::PermissionsExt;
    // A Note without a mode to inherit is the user's own document: 0600,
    // the same as the temporary it was written through, and never wider
    // than the umask this process runs under. Vault folders are the
    // user's and are not tightened, so 0644 here read to every other
    // account on a shared machine.
    let permissions = permissions.unwrap_or_else(|| fs::Permissions::from_mode(0o600));
    if let Err(err) = fs::set_permissions(path, permissions) {
        log::warn!(
            "failed to preserve Note permissions on {}: {err:#}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn set_user_document_permissions(_path: &Path, _permissions: Option<fs::Permissions>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn new_notes_are_private_and_existing_permissions_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        write_bytes_atomic(&path, b"private note").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        write_bytes_atomic(&path, b"updated note").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
    }

    #[test]
    fn creates_and_reuses_a_vault_document_session() {
        let temp = tempfile::tempdir().unwrap();
        let first = open_vault_document(temp.path(), "Design/Overview.md", true).unwrap();
        let second = open_vault_document(temp.path(), "Design/Overview.md", false).unwrap();
        assert!(Arc::ptr_eq(&first.session, &second.session));
        assert_eq!(second.relative_path, "Design/Overview.md");
        assert!(second.document_path.ends_with("Design/Overview.md"));
    }

    #[test]
    fn concurrent_open_keeps_one_canonical_document_session() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("Large.md"),
            "paragraph\n".repeat(64 * 1024),
        )
        .unwrap();
        let root = Arc::new(temp.path().to_path_buf());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let open = |root: Arc<PathBuf>, barrier: Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                barrier.wait();
                open_vault_document(&root, "Large.md", false).unwrap()
            })
        };
        let first = open(Arc::clone(&root), Arc::clone(&barrier));
        let second = open(root, Arc::clone(&barrier));
        barrier.wait();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert!(Arc::ptr_eq(&first.session, &second.session));
    }

    #[test]
    fn vault_file_index_includes_attachments_but_markdown_index_does_not() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("assets")).unwrap();
        fs::write(temp.path().join("Note.md"), "# Note").unwrap();
        fs::write(temp.path().join("assets/manual.pdf"), b"pdf").unwrap();
        assert_eq!(
            vault_file_paths(temp.path()).unwrap(),
            vec!["assets/manual.pdf", "Note.md"]
        );
        assert_eq!(vault_markdown_paths(temp.path()).unwrap(), vec!["Note.md"]);
    }

    #[test]
    fn imports_supported_image_with_collision_safe_name() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("My image.PNG");
        fs::write(&source, b"not decoded by the store").unwrap();
        let document = open_vault_document(temp.path(), "Notes/Today.md", true).unwrap();
        assert_eq!(
            import_attachment(&document, &source).unwrap(),
            "attachments/my-image.png"
        );
        assert_eq!(
            import_attachment(&document, &source).unwrap(),
            "attachments/my-image-2.png"
        );
    }

    #[test]
    fn local_image_resolution_rejects_escape() {
        let temp = tempfile::tempdir().unwrap();
        let document = open_vault_document(temp.path(), "Today.md", true).unwrap();
        let outside = temp.path().join("outside.png");
        fs::write(&outside, b"x").unwrap();
        assert!(resolve_local_image(
            &document.vault_root,
            &document.document_path,
            "../../../outside.png"
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn notebook_document_path_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let content = temp.path().join("content");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&content).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, content.join("linked")).unwrap();
        assert!(safe_content_path(&content, Path::new("linked/Note.md")).is_err());
    }

    #[test]
    fn serialized_saves_write_the_latest_shared_revision() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Inbox.md");
        let session = Arc::new(Mutex::new(MarkdownDocumentSession::new(
            "doc".into(),
            path.clone(),
            "a".into(),
        )));
        let mut first_view = super::super::EditorViewState::default();
        session.lock().set_caret(&mut first_view, 1, false);
        session.lock().insert_text(&mut first_view, "b");

        let save_lock = session.lock().save_lock();
        let guard = save_lock.lock();
        let first_session = Arc::clone(&session);
        let first = std::thread::spawn(move || save_document_revision(&first_session));
        session.lock().insert_text(&mut first_view, "c");
        let second_session = Arc::clone(&session);
        let second = std::thread::spawn(move || save_document_revision(&second_session));
        drop(guard);

        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "abc");
        assert!(!session.lock().is_dirty());
    }
}
