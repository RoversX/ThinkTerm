<script lang="ts">
  // The window's tab row: the sidebar toggle, a capsule per tab of this
  // window, the new-tab button and the right panel's toggle. What
  // `chrome::TabStrip::render` drew.
  import { onChromeClick } from './chrome';
  import { s, views } from './client.svelte';
  import { panelLeft, panelRight, plus, squareTerminal, x } from './icons';
  import { openMenu } from './menu.svelte';
  import { armDrag } from './drag.svelte';
  import { short } from './title';

  const tabs = $derived(views.tabs?.tabs ?? []);
  const controls = $derived(views.tabs?.controls ?? null);

  // A press on a capsule -- its close button included -- is that tab's.
  function onMenu(ev: MouseEvent) {
    const target = ev.target;
    if (!(target instanceof Element)) return;
    const tab = target.closest('.tab')?.getAttribute('data-tab');
    if (tab === null || tab === undefined) return;
    ev.preventDefault();
    openMenu('tab', tab, ev.clientX, ev.clientY);
  }

  // A press on a capsule may be its drag; the close button is its own, and
  // a press that never travels is still the click that switches tab.
  function onPointerDown(ev: PointerEvent) {
    const target = ev.target;
    if (!(target instanceof Element) || target.closest('.x')) return;
    const cap = target.closest('.tab');
    const id = cap?.getAttribute('data-tab');
    if (!cap || id === null || id === undefined) return;
    armDrag(ev, { kind: 'tab', id, label: cap.textContent?.trim() ?? id });
  }
</script>

<!-- The row is one delegated handler, as the wasm had it; its children
     are the controls, not the container. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div id="tabs" onclick={onChromeClick} oncontextmenu={onMenu} onpointerdown={onPointerDown}>
  <span class="act" data-action="sidebar" title={s('web-tip-sidebar')}>{@html panelLeft}</span>
  {#each tabs as tab (tab.tab)}
    <span class="tab" class:current={tab.current} data-pane={tab.target} data-tab={tab.tab} title={tab.title}>
      {@html squareTerminal}<span class="t">{short(tab.label)}</span><span
        class="x"
        class:danger={controls?.closing === tab.tab}
        data-action="close-tab"
        data-tab={tab.tab}
        title={s('web-tip-close-tab')}
      >{#if controls?.closing === tab.tab}{s('web-confirm-close')}{:else}{@html x}{/if}</span>
    </span>
  {/each}
  <span class="act" data-action="new-tab" title={s('web-tip-new-tab')}>{@html plus}</span>
  <!-- The row's right end, as the desktop's tab bar has it: the right
       panel's toggle. -->
  <span class="trail">
    <span
      class="act"
      class:on={views.settings['agents-panel']}
      data-action="agents"
      title={s('web-tip-agents')}>{@html panelRight}</span>
  </span>
</div>
