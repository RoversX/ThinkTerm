use super::MarkdownDocumentSession;
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tempfile::NamedTempFile;
use uuid::Uuid;

pub(crate) type NotebookId = String;
pub(crate) type DocumentId = String;

const STORE_VERSION: u32 = 1;
const MAX_IMPORTED_IMAGE_BYTES: u64 = 25 * 1024 * 1024;
const SUPPORTED_IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registry {
    version: u32,
    default_notebook_id: NotebookId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DocumentRecord {
    id: DocumentId,
    path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NotebookMetadata {
    version: u32,
    id: NotebookId,
    name: String,
    active_document_id: DocumentId,
    documents: Vec<DocumentRecord>,
}

#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct DefaultNotebook {
    pub notebook_id: NotebookId,
    pub document_id: DocumentId,
    pub root: PathBuf,
    pub content_root: PathBuf,
    pub document_path: PathBuf,
    pub session: Arc<Mutex<MarkdownDocumentSession>>,
}

lazy_static::lazy_static! {
    static ref DEFAULT_NOTEBOOK: Mutex<Option<DefaultNotebook>> = Mutex::new(None);
    static ref ATTACHMENT_IMPORT_LOCK: Mutex<()> = Mutex::new(());
}

pub(crate) fn default_notebook() -> Result<DefaultNotebook> {
    let mut slot = DEFAULT_NOTEBOOK.lock();
    if let Some(notebook) = slot.as_ref() {
        return Ok(notebook.clone());
    }
    let notebook = ensure_default_notebook_at(&crate::native_paths::data_dir().join("notes/v1"))?;
    *slot = Some(notebook.clone());
    Ok(notebook)
}

pub(crate) fn default_document_session() -> Result<Arc<Mutex<MarkdownDocumentSession>>> {
    Ok(default_notebook()?.session)
}

fn ensure_default_notebook_at(base: &Path) -> Result<DefaultNotebook> {
    fs::create_dir_all(base).with_context(|| format!("create {}", base.display()))?;
    let registry_path = base.join("registry.json");
    let registry = if registry_path.exists() {
        read_json::<Registry>(&registry_path)?
    } else {
        let registry = Registry {
            version: STORE_VERSION,
            default_notebook_id: new_id(),
        };
        write_json_atomic(&registry_path, &registry)?;
        registry
    };

    if registry.version != STORE_VERSION {
        bail!("unsupported Note registry version {}", registry.version);
    }

    let root = base.join("notebooks").join(&registry.default_notebook_id);
    let content_root = root.join("content");
    let attachments = content_root.join("attachments");
    fs::create_dir_all(&attachments)
        .with_context(|| format!("create {}", attachments.display()))?;
    ensure_canonical_descendant(&content_root, &attachments)?;

    let metadata_path = root.join("notebook.json");
    let metadata = if metadata_path.exists() {
        read_json::<NotebookMetadata>(&metadata_path)?
    } else {
        let document_id = new_id();
        let metadata = NotebookMetadata {
            version: STORE_VERSION,
            id: registry.default_notebook_id.clone(),
            name: "Notes".to_string(),
            active_document_id: document_id.clone(),
            documents: vec![DocumentRecord {
                id: document_id,
                path: "Inbox.md".to_string(),
            }],
        };
        write_json_atomic(&metadata_path, &metadata)?;
        metadata
    };

    if metadata.version != STORE_VERSION || metadata.id != registry.default_notebook_id {
        bail!("invalid default Notebook metadata");
    }
    let document = metadata
        .documents
        .iter()
        .find(|document| document.id == metadata.active_document_id)
        .or_else(|| metadata.documents.first())
        .context("default Notebook has no document")?;
    let document_path = safe_content_path(&content_root, Path::new(&document.path))?;
    if !document_path.exists() {
        write_bytes_atomic(&document_path, b"")?;
    }
    let source = fs::read_to_string(&document_path)
        .with_context(|| format!("read {}", document_path.display()))?;
    let session = Arc::new(Mutex::new(MarkdownDocumentSession::new(
        document.id.clone(),
        document_path.clone(),
        source,
    )));
    Ok(DefaultNotebook {
        notebook_id: registry.default_notebook_id,
        document_id: document.id.clone(),
        root,
        content_root,
        document_path,
        session,
    })
}

pub(crate) fn save_document_revision(session: &Arc<Mutex<MarkdownDocumentSession>>) -> Result<u64> {
    let save_lock = session.lock().save_lock();
    let _save_guard = save_lock.lock();
    let (revision, path, source) = session.lock().snapshot_for_save();
    let result = write_bytes_atomic(&path, source.as_bytes());
    let finish_result = result
        .as_ref()
        .map(|_| ())
        .map_err(|err| anyhow::anyhow!("{err:#}"));
    session.lock().finish_save(revision, finish_result);
    result.map(|_| revision)
}

pub(crate) fn import_attachment(notebook: &DefaultNotebook, source: &Path) -> Result<String> {
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
    let attachment_dir = notebook.content_root.join("attachments");
    fs::create_dir_all(&attachment_dir)
        .with_context(|| format!("create {}", attachment_dir.display()))?;
    ensure_canonical_descendant(&notebook.content_root, &attachment_dir)?;
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
    set_private_permissions(&destination);

    let relative = destination
        .strip_prefix(&notebook.content_root)
        .context("attachment escaped Notebook content")?;
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
        bail!("invalid Notebook document path {}", relative.display());
    }
    let candidate = root.join(relative);
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("resolve {}", root.display()))?;
    let mut existing = candidate.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .context("Notebook document path has no existing parent")?;
    }
    let canonical_existing = existing
        .canonicalize()
        .with_context(|| format!("resolve {}", existing.display()))?;
    if !canonical_existing.starts_with(&canonical_root) {
        bail!("Notebook document path escapes content root");
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
        bail!("{} escapes Notebook content root", path.display());
    }
    Ok(())
}

fn new_id() -> String {
    Uuid::new_v4().simple().to_string()
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

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("serialize Note metadata")?;
    write_bytes_atomic(path, &bytes)
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Note path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
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
    set_private_permissions(path);
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        log::warn!(
            "failed to set private Note permissions on {}: {err:#}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_reopens_stable_default_notebook() {
        let temp = tempfile::tempdir().unwrap();
        let first = ensure_default_notebook_at(temp.path()).unwrap();
        first.session.lock().source();
        let second = ensure_default_notebook_at(temp.path()).unwrap();
        assert_eq!(first.notebook_id, second.notebook_id);
        assert_eq!(first.document_id, second.document_id);
        assert!(second.document_path.ends_with("Inbox.md"));
    }

    #[test]
    fn imports_supported_image_with_collision_safe_name() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("My image.PNG");
        fs::write(&source, b"not decoded by the store").unwrap();
        let store_root = temp.path().join("store");
        let notebook = ensure_default_notebook_at(&store_root).unwrap();
        assert_eq!(
            import_attachment(&notebook, &source).unwrap(),
            "attachments/my-image.png"
        );
        assert_eq!(
            import_attachment(&notebook, &source).unwrap(),
            "attachments/my-image-2.png"
        );
    }

    #[test]
    fn local_image_resolution_rejects_escape() {
        let temp = tempfile::tempdir().unwrap();
        let store_root = temp.path().join("store");
        let notebook = ensure_default_notebook_at(&store_root).unwrap();
        let outside = temp.path().join("outside.png");
        fs::write(&outside, b"x").unwrap();
        assert!(resolve_local_image(
            &notebook.content_root,
            &notebook.document_path,
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
