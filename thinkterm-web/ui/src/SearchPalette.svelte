<script lang="ts">
  // The search palette: a field, and what the wasm ranked for what is in
  // it, in sections. What can be found and what a pick does are the
  // wasm's (thinkterm-web/src/palette.rs); drawn here, walked with the
  // arrows, and closed by anything that means the page moved on.
  import { iconByName } from './icons';
  import { closePalette, move, palette, run, search, select, selectedId, shownSections } from './palette.svelte';
  import { POP, ms } from './motion';
  import { fade, scale } from 'svelte/transition';

  const sections = $derived(shownSections());
  const current = $derived(selectedId());

  /** The field is opened to be typed in. */
  function typeHere(node: HTMLInputElement) {
    node.focus();
    node.select();
  }

  function onInput(ev: Event) {
    const field = ev.currentTarget;
    if (field instanceof HTMLInputElement) search(field.value);
  }

  // Nothing typed at the palette reaches the terminal.
  function onKeydown(ev: KeyboardEvent) {
    ev.stopPropagation();
    switch (ev.key) {
      case 'Escape': closePalette(); break;
      case 'ArrowDown': move(1); break;
      case 'ArrowUp': move(-1); break;
      case 'Enter': run(current); break;
      // The field is the only thing to move to, so Tab would only take
      // the keyboard out of the palette.
      case 'Tab': break;
      default: return;
    }
    ev.preventDefault();
  }

  let list = $state<HTMLDivElement | null>(null);

  // The keyboard can walk past the bottom of the list; the row it is on is
  // brought back into sight rather than left off-screen.
  $effect(() => {
    const id = current;
    if (!list || id === '') return;
    list.querySelector(`.pe[data-id="${CSS.escape(id)}"]`)?.scrollIntoView({ block: 'nearest' });
  });
</script>

<!-- The rows are the palette's, not buttons; the keyboard stays in the
     field, which is what the pointer selecting a row keeps true. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
{#if palette.open}
  <div id="palette-back" onpointerdown={closePalette} transition:fade={{ duration: ms(POP) }}></div>
  <div
    id="palette"
    role="dialog"
    aria-modal="true"
    transition:scale={{ duration: ms(POP), start: 0.97, opacity: 0 }}
  >
    <input
      class="q"
      value={palette.query}
      placeholder={palette.results.placeholder}
      spellcheck="false"
      autocomplete="off"
      autocapitalize="off"
      autocorrect="off"
      oninput={onInput}
      onkeydown={onKeydown}
      use:typeHere
    />
    <div class="pl" bind:this={list}>
      {#each sections as section (section.group)}
        <div class="ph">{section.label}</div>
        {#each section.entries as entry (entry.id)}
          <div
            class="pe"
            class:on={entry.id === current}
            data-id={entry.id}
            onpointerenter={() => select(entry.id)}
            onclick={() => run(entry.id)}
          >
            <span class="ic">{#if entry.icon}{@html iconByName(entry.icon) ?? ''}{/if}</span><span
              class="t">{entry.title}</span
            >{#if entry.subtitle}<span class="st">{entry.subtitle}</span>{/if}{#if entry.accessory}<span
              class="acc">{entry.accessory}</span
            >{/if}
          </div>
        {/each}
      {:else}
        <div class="pn">{palette.results.empty}</div>
      {/each}
    </div>
  </div>
{/if}
