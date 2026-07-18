//! Native, host-independent Markdown document/editor primitives.
//!
//! The right-sidebar Note is the first host, but this module deliberately has
//! no dependency on right-sidebar state so that a full content view and
//! read-only previews can share the same source/projection model.

mod document;
mod host;
mod projection;
mod store;
mod surface;

pub(crate) use document::{
    Affinity, EditorMode, EditorViewState, MarkdownDocumentSession, SaveState, SourcePosition,
    SourceSelection,
};
pub(crate) use host::{NoteCodeBlockLayout, NoteHostState, NoteLineLayout, NoteRunLayout};
pub(crate) use projection::{
    BlockKind, InlineStyle, MarkdownProjection, ProjectedCodeBlock, ProjectedObject, TableAlignment,
};
pub(crate) use store::{
    default_document_session, default_notebook, import_attachment, resolve_local_image,
    save_document_revision,
};
pub(crate) use surface::{
    build_visual_document, fit_table_columns, wrap_visual_document_by_width, VisualDocument,
    VisualLineKind,
};
