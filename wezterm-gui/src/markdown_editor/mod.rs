//! Native, host-independent Markdown document/editor primitives.
//!
//! The right-sidebar Note is the first host, but this module deliberately has
//! no dependency on right-sidebar state so that a full content view and
//! read-only previews can share the same source/projection model.

mod document;
mod host;
pub(crate) mod mermaid;
mod projection;
mod remote_image;
mod spellcheck;
mod store;
mod surface;

pub(crate) use document::{
    Affinity, DocumentSnapshot, EditorMode, EditorViewState, MarkdownDocumentSession, SaveState,
    SelectionGranularity, SourcePosition, SourceSelection,
};
pub(crate) use host::{
    AutosaveWakeAction, NoteCodeBlockLayout, NoteHostState, NoteLineGeometry, NoteLineLayout,
    NoteRunLayout,
};
pub(crate) use projection::{
    BlockKind, InlineStyle, MarkdownProjection, ProjectedCodeBlock, ProjectedObject, TableAlignment,
};
pub(crate) use remote_image::load_remote_image;
pub(crate) use spellcheck::{build_spell_check_chunks_in_range, NoteSpellingIssue};
pub(crate) use store::{
    import_attachment, open_vault_document, resolve_local_image, save_document_revision,
    vault_file_paths, vault_markdown_paths, VaultDocument,
};
#[cfg(test)]
pub(crate) use surface::wrap_visual_document_by_width;
pub(crate) use surface::{
    build_visual_document, fit_table_columns, wrap_visual_document_by_width_cached, VisualDocument,
    VisualLineKind, VisualWrapCache,
};
