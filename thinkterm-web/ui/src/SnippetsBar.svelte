<script lang="ts">
  // The Snippets tab's toolbar line, where the Agents tab has its summary:
  // the search and the new-snippet button over the list, and over the
  // editor its way back, its title and Save -- the desktop's toolbar
  // (`paint_snippets_toolbar`, `paint_snippet_editor`).
  import { s, views } from './client.svelte';
  import { arrowLeft, plus, search as searchIcon } from './icons';
  import { closeEditor, editor, openNew, saveEditor, search } from './snippets.svelte';

  const query = $derived(views.snippets.query);
</script>

{#if editor.open}
  <div class="sbar">
    <button class="sb-icon" type="button" title={s('right-cancel')} aria-label={s('right-cancel')} onclick={closeEditor}>{@html arrowLeft}</button>
    <span class="sb-title">{editor.id === null ? s('right-new-snippet') : s('right-edit-snippet')}</span>
    <button class="sb-save" type="button" disabled={editor.saving} onclick={saveEditor}>{s('right-save')}</button>
  </div>
{:else}
  <div class="sbar">
    <label class="sb-search">
      <span class="sb-glass">{@html searchIcon}</span>
      <input
        type="search"
        placeholder={s('right-search')}
        value={query}
        autocomplete="off"
        spellcheck="false"
        oninput={(ev) => search(ev.currentTarget.value)}
        onkeydown={(ev) => {
          if (ev.key === 'Escape' && ev.currentTarget.value !== '') {
            ev.preventDefault();
            search('');
          }
        }}
      />
    </label>
    <button class="sb-icon" type="button" title={s('right-new-snippet')} aria-label={s('right-new-snippet')} onclick={openNew}>{@html plus}</button>
  </div>
{/if}
