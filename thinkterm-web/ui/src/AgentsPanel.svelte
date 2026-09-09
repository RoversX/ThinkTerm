<script lang="ts">
  // The desktop's right-hand Agents panel: one row per pane a coding agent
  // runs in, with its live state. What `agent_panel.rs` paints, from the
  // rows the wasm publishes (thinkterm-web/src/agents.rs); a click on a row
  // brings that pane on show.
  import { handle } from './client';
  import { refreshViews, s, views } from './client.svelte';
  import { agentIcon, circleAlert, circleCheck, loaderCircle } from './icons';

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

  // A click anywhere in a row is that row's, as it is on the desktop: the
  // whole row is the target, not a button inside it.
  function onClick(ev: MouseEvent) {
    const target = ev.target;
    if (!(target instanceof Element)) return;
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
  // the desktop gives its right sidebar.
  const STORE = 'thinkterm.agents-width';
  const MIN = 200;
  const MAX = 360;
  const DEFAULT = 260;
  let width = DEFAULT;

  function setWidth(px: number): number {
    const w = Math.round(Math.min(MAX, Math.max(MIN, px)));
    document.documentElement.style.setProperty('--agents-w', `${w}px`);
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
  let frozen: HTMLCanvasElement | null = null;

  function onPointerDown(ev: PointerEvent) {
    const target = ev.target;
    if (!(target instanceof HTMLElement) || !target.classList.contains('handle')) return;
    ev.preventDefault();
    dragging = true;
    try {
      target.setPointerCapture(ev.pointerId);
    } catch {
      // Without capture the drag still follows the pointer over the panel.
    }
    // As the sidebar's edge does: the terminal is resized from the canvas's
    // box, so a drag that moved it would resize the whole grid on every
    // frame. Its size is pinned for the drag, and the one real resize
    // happens on release.
    const canvas = document.getElementById('term');
    if (canvas instanceof HTMLCanvasElement) {
      const box = canvas.getBoundingClientRect();
      canvas.style.width = `${box.width}px`;
      canvas.style.height = `${box.height}px`;
      frozen = canvas;
    }
  }

  // The panel is against the right edge, so its width is what is left of the
  // window to the right of the pointer.
  function onPointerMove(ev: PointerEvent) {
    if (dragging) width = setWidth(window.innerWidth - ev.clientX);
  }

  function onPointerUp() {
    if (!dragging) return;
    dragging = false;
    try {
      localStorage.setItem(STORE, String(width));
    } catch {
      // As above: the width is then this page load's only.
    }
    if (frozen) {
      frozen.style.width = '';
      frozen.style.height = '';
      frozen = null;
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
>
  <div class="handle"></div>
  <div class="hd">
    <span class="ti">{s('web-agents-title')}</span>
    <span class="sum">{summary}</span>
  </div>
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
</aside>
