<script lang="ts">
  // The Snippets tab below its toolbar: the cards, as the desktop draws
  // them (`paint_snippets_list`), or the editor. A card's Run and Paste put
  // it into the focused pane; a press anywhere else on it edits it.
  import { s, views } from './client.svelte';
  import { circleAlert, codeXml, trash2 } from './icons';
  import { armed, closeEditor, editor, openExisting, paste, pressDelete, saveEditor } from './snippets.svelte';

  const view = $derived(views.snippets);

  function onCardClick(ev: MouseEvent, id: string) {
    // The buttons on the card are their own.
    if (ev.target instanceof Element && ev.target.closest('button')) return;
    openExisting(id);
  }

  function onEditorKey(ev: KeyboardEvent) {
    if (ev.key === 'Escape') {
      ev.preventDefault();
      closeEditor();
    } else if (ev.key === 'Enter' && (ev.metaKey || ev.ctrlKey)) {
      ev.preventDefault();
      saveEditor();
    }
  }
</script>

{#if editor.open}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="list snippet-editor" onkeydown={onEditorKey}>
    <label class="se-label" for="snippet-title">{s('right-action-description')}</label>
    <input
      id="snippet-title"
      class="se-field"
      type="text"
      placeholder={s('right-action-description-placeholder')}
      autocomplete="off"
      bind:value={editor.title}
    />
    <label class="se-label" for="snippet-body">{s('right-script-required')}</label>
    <textarea
      id="snippet-body"
      class="se-field se-body"
      class:want={editor.wantBody}
      placeholder={s('right-script-placeholder')}
      autocapitalize="off"
      autocomplete="off"
      spellcheck="false"
      bind:value={editor.body}
      oninput={() => (editor.wantBody = false)}
    ></textarea>
  </div>
{:else}
  <div class="list snippets">
    {#if view.rows.length === 0}
      {#if view.state !== 'loading'}
        <!-- Nothing yet while the host sends them: saying there are none
             would be wrong for the moment it takes. -->
        <div class="sn-empty" title={view.reason ?? ''}>
          <span class="ic">{@html view.state === 'unavailable' ? circleAlert : codeXml}</span>
          <span class="sn-empty-t">{view.empty}</span>
        </div>
      {/if}
    {:else}
      {#each view.rows as row (row.id)}
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div class="sn" data-snippet={row.id} onclick={(ev) => onCardClick(ev, row.id)}>
          <div class="sn-top">
            <span class="sn-title">{row.title}</span>
            <span class="sn-actions">
              <button type="button" class="sn-btn" onclick={() => paste(row.id, true)}>{s('right-run')}</button>
              <button type="button" class="sn-btn" onclick={() => paste(row.id, false)}>{s('menu-paste')}</button>
              <button
                type="button"
                class="sn-btn sn-del"
                class:armed={armed.id === row.id}
                title={s('web-tip-delete-snippet')}
                aria-label={s('web-tip-delete-snippet')}
                onclick={() => pressDelete(row.id)}
              >{#if armed.id === row.id}{s('web-confirm-delete')}{:else}{@html trash2}{/if}</button>
            </span>
          </div>
          <span class="sn-preview">{row.preview}</span>
        </div>
      {/each}
    {/if}
  </div>
{/if}
