// The Snippets tab's page-side state: the editor, and a delete pressed
// once. The snippets are the plugin host's, which does everything with
// them (thinkterm-snippets `wire`); the page asks through the wasm
// (thinkterm-web/src/app.rs) and shows what comes back. This only holds
// what is being typed until it is saved.

import { tick } from 'svelte';
import { handle } from './client';
import { refreshViews } from './client.svelte';
import { focusTerminal } from './mobile.svelte';

export const editor = $state({
  /** Whether the editor is up rather than the list. */
  open: false,
  /** The snippet being edited; null for a new one. */
  id: null as string | null,
  title: '',
  body: '',
  /** The host found nothing to save in the script: the field says so. */
  wantBody: false,
  /** A save is on its way: another press waits for its answer rather
      than saving a new snippet twice. */
  saving: false,
});

/** A delete pressed once, and when; the second press within the window
    deletes, as the sidebar's thread delete does. */
export const armed = $state({ id: null as string | null, at: 0 });
const CONFIRM_MS = 3000;

async function focusField(id: string) {
  await tick();
  document.getElementById(id)?.focus();
}

export function openNew() {
  editor.open = true;
  editor.id = null;
  editor.title = '';
  editor.body = '';
  editor.wantBody = false;
  focusField('snippet-title');
}

/** The editor, on a snippet the host sends whole. */
export async function openExisting(id: string) {
  const raw = await handle.client?.snippet(id);
  if (typeof raw !== 'string' || raw === 'null') return;
  const snippet = JSON.parse(raw) as { id: string; title: string; body: string };
  editor.open = true;
  editor.id = snippet.id;
  editor.title = snippet.title;
  editor.body = snippet.body;
  editor.wantBody = false;
  focusField('snippet-title');
}

export function closeEditor() {
  editor.open = false;
  editor.id = null;
  editor.title = '';
  editor.body = '';
  editor.wantBody = false;
}

type SaveOutcome =
  | { outcome: 'saved'; id: string }
  | { outcome: 'empty' }
  | { outcome: 'gone' }
  | { outcome: 'error'; message: string };

/** What the editor holds, to the host; the editor follows its answer, as
    the desktop's does. */
export async function saveEditor() {
  const client = handle.client;
  if (!client || editor.saving) return;
  editor.saving = true;
  try {
    const raw = await client.snippet_save(editor.id ?? undefined, editor.title, editor.body);
    const saved = JSON.parse(raw as string) as SaveOutcome;
    switch (saved.outcome) {
      case 'saved':
        closeEditor();
        break;
      case 'empty':
        editor.wantBody = true;
        focusField('snippet-body');
        break;
      case 'gone':
        // Deleted meanwhile: what was typed is kept, as a new one.
        editor.id = null;
        break;
      case 'error':
        console.warn(`snippets: ${saved.message}`);
        break;
    }
  } finally {
    editor.saving = false;
  }
  refreshViews();
}

/** The first press arms the button; the second, soon after, deletes. */
export async function pressDelete(id: string) {
  const now = performance.now();
  if (armed.id === id && now - armed.at < CONFIRM_MS) {
    armed.id = null;
    await handle.client?.snippet_delete(id);
    refreshViews();
    return;
  }
  armed.id = id;
  armed.at = now;
  setTimeout(() => {
    if (armed.id === id && performance.now() - armed.at >= CONFIRM_MS) armed.id = null;
  }, CONFIRM_MS);
}

/** Into the focused pane, as the host says to send it, and the keyboard
    back to the terminal, so a pasted command can be run with Enter --
    where that is wanted: a phone keeps its soft keyboard down
    (`focusTerminal`). */
export async function paste(id: string, run: boolean) {
  if (await handle.client?.snippet_paste(id, run)) focusTerminal();
}

export function search(query: string) {
  handle.client?.snippets_search(query);
  refreshViews();
}
