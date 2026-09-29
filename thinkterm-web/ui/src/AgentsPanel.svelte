<script lang="ts">
  // The desktop's right-hand panel: its Agents tab, one row per pane a
  // coding agent runs in with its live state, as `agent_panel.rs` paints
  // it from the rows the wasm publishes (thinkterm-web/src/agents.rs), a
  // click on a row bringing that pane on show; its Snippets tab
  // (SnippetsBar, SnippetsList); and the panels plugins draw
  // (PluginPanel), with the extended view one asks for left of it
  // (PluginExtended).
  import PluginExtended from './PluginExtended.svelte';
  import PluginPanel from './PluginPanel.svelte';
  import SnippetsBar from './SnippetsBar.svelte';
  import SnippetsList from './SnippetsList.svelte';
  import { handle } from './client';
  import { refreshViews, s, views } from './client.svelte';
  import { agentIcon, circleAlert, circleCheck, iconByName, loaderCircle } from './icons';

  // The desktop's segmented mode selector. Which tabs exist, which this
  // browser can open and what each is called are the model's
  // (thinkterm-web/src/agents.rs `panel_tabs`); drawn here.
  const tabs = $derived(views.agents.tabs);
  const active = $derived(views.agents.active);
  const plugin = $derived(active.startsWith('plugin:'));
  const heading = $derived(tabs.find((tab) => tab.id === active)?.label ?? s('web-agents-title'));

  const rows = $derived(views.agents.rows);
  const summary = $derived(views.agents.summary);

  // The mark on the body is what makes the tab row, the canvas and the pane
  // bars leave room on the right; it is the panel's own, so it goes up when
  // the panel is mounted and comes off with it.
  $effect(() => {
    document.body.dataset.agents = '';
    return () => {
      delete document.body.dataset.agents;
    };
  });

  // What the panel shows is followed only while it is up: the snippets,
  // and the server's connection to the plugin host for them, go with it.
  $effect(() => {
    handle.client?.set_right_panel_shown(true);
    return () => handle.client?.set_right_panel_shown(false);
  });

  // A click anywhere in a row is that row's, as it is on the desktop: the
  // whole row is the target, not a button inside it.
  function onClick(ev: MouseEvent) {
    const target = ev.target;
    if (!(target instanceof Element)) return;
    const mode = target.closest('.mode:not(.off)')?.getAttribute('data-mode');
    if (mode) {
      if (mode !== active && handle.client?.set_right_panel(mode)) refreshViews();
      return;
    }
    const raw = target.closest('.ag')?.getAttribute('data-pane');
    if (raw === null || raw === undefined) return;
    const pane = Number(raw);
    if (!Number.isSafeInteger(pane) || pane < 0) return;
    ev.preventDefault();
    // The reveal may have switched tab or window; what it changed is on
    // show before this returns, as a menu row's outcome is.
    if (handle.client?.agent_reveal(pane)) refreshViews();
  }

  // The panel's width: a preference of this browser's, within the bounds
  // the desktop gives its right sidebar, which is a wider panel than the
  // left one -- 340px at its narrowest (RIGHT_SIDEBAR_MIN_WIDTH), because
  // its rows carry two lines. A browser window has less to spare than a
  // desktop one, so the floor is lower and the default is the desktop's
  // own minimum.
  const STORE = 'thinkterm.agents-width';
  const MIN = 240;
  const MAX = 520;
  const DEFAULT = 300;
  let width = DEFAULT;

  function setWidth(px: number): number {
    const w = Math.round(Math.min(MAX, Math.max(MIN, px)));
    document.documentElement.style.setProperty('--agents-w', `${w}px`);
    handle.client?.resize();
    return w;
  }

  $effect(() => {
    let stored: number | null = null;
    try {
      const raw = localStorage.getItem(STORE);
      if (raw !== null && raw !== '' && Number.isFinite(Number(raw))) stored = Number(raw);
    } catch {
      // A browser that blocks storage keeps the default; not worth a remark.
    }
    width = setWidth(stored ?? DEFAULT);
  });

  let dragging = false;
  /** The canvas whose box is held still for the drag, so it resizes once. */

  function onPointerDown(ev: PointerEvent) {
    const target = ev.target;
    if (!(target instanceof HTMLElement) || !target.classList.contains('handle')) return;
    ev.preventDefault();
    dragging = true;
    handle.client?.panel_drag(true);
    try {
      target.setPointerCapture(ev.pointerId);
    } catch {
      // Without capture the drag still follows the pointer over the panel.
    }
    // The canvas follows the panel's edge as it moves: the grid is
    // resized live, as the desktop's is.
  }

  // The panel is against the right edge, so its width is what is left of the
  // window to the right of the pointer.
  function onPointerMove(ev: PointerEvent) {
    if (!dragging) return;
    // A release the page never saw (over another window, say) ends the
    // drag on the next move with no button down.
    if (ev.buttons === 0) {
      onPointerUp();
      return;
    }
    width = setWidth(window.innerWidth - ev.clientX);
    handle.client?.resize();
  }

  function onPointerUp() {
    if (!dragging) return;
    dragging = false;
    handle.client?.panel_drag(false);
    try {
      localStorage.setItem(STORE, String(width));
    } catch {
      // As above: the width is then this page load's only.
    }
  }
</script>

<!-- One delegated handler, as the sidebar has; its children are the rows,
     not the container. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<aside
  id="agents"
  onclick={onClick}
  onpointerdown={onPointerDown}
  onpointermove={onPointerMove}
  onpointerup={onPointerUp}
  onpointercancel={onPointerUp}
  onlostpointercapture={onPointerUp}
>
  <div class="handle"></div>
  <div class="hd">
    <span class="ti">{heading}</span>
    <!-- The desktop's selector: a segment a tab, the active one a filled
         pill carrying its label, the rest icon-only; past five tabs the
         label has no room, and the active one is its icon too. -->
    <div class="modes" class:icons={!views.agents.labeled} role="tablist">
      {#each tabs as tab (tab.id)}
        <span
          class="mode"
          class:on={tab.id === active}
          class:off={!tab.available}
          role="tab"
          aria-selected={tab.id === active}
          aria-disabled={!tab.available}
          data-mode={tab.id}
          title={tab.tip}
        >{@html iconByName(tab.icon) ?? ''}{#if tab.id === active && views.agents.labeled}<span class="ml">{tab.label}</span>{/if}</span>
      {/each}
    </div>
    {#if active === 'snippets'}
      <SnippetsBar />
    {:else if !plugin}
      <span class="sum">{summary}</span>
    {/if}
  </div>
  {#if active === 'snippets'}
    <SnippetsList />
  {:else if plugin}
    {#key active}
      <PluginPanel />
    {/key}
  {:else}
  <div class="list">
    {#each rows as row (row.pane)}
      <div class="ag" class:elsewhere={!row.here} data-pane={row.pane} title={row.place}>
        <span class="ic">{@html agentIcon(row.icon)}</span>
        <span class="tx">
          <span class="n">{row.name}{#if row.title !== ''}<span class="d"> · {row.title}</span>{/if}</span>
          <span class="p">{row.state_label}{#if row.place !== ''} · {row.place}{/if}</span>
        </span>
        {#if row.state === 'working'}
          <span class="st spin">{@html loaderCircle}</span>
        {:else if row.state === 'blocked'}
          <span class="st alert">{@html circleAlert}</span>
        {:else if row.state === 'idle'}
          <span class="st idle">{@html circleCheck}</span>
        {/if}
      </div>
    {/each}
  </div>
  {/if}
</aside>
{#if plugin && views.panelExtended}
  {#key active}
    <PluginExtended plugin={active.slice('plugin:'.length)} />
  {/key}
{/if}
