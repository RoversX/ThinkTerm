<script lang="ts">
  // The desktop's left panel: Spaces, Projects and Threads, plus the
  // windows that belong to no thread. What `sidebar::html` drew, from the
  // rows the wasm publishes; the clicks and the inline name go back to it.
  import { s, views, sideClick, sideKey } from './client.svelte';
  import {
    archive, archiveRestore, chevronDown, chevronRight, circleAlert, circleCheck, circlePlus,
    bell, ellipsis, folder, folderOpen, folderPlus, layers, loaderCircle, pin, pinOff, plus,
    trash2,
  } from './icons';
  import { menu, openMenu } from './menu.svelte';
  import { search as searchIcon, settings as settingsIcon } from './icons';
  import { openPalette } from './palette.svelte';
  import { openSettings } from './settings.svelte';
  import { mobile, openSide } from './mobile.svelte';
  import { palette } from './palette.svelte';
  import { followSpace, panel } from './settings.svelte';
  import type { SideRow } from './model';

  const rows = $derived(views.sidebar.rows);
  const editing = $derived(views.sidebar.editing);
  const editingThread = (id: string) => editing.kind === 'thread' && editing.id === id;
  const editingProject = (id: string) => editing.kind === 'project' && editing.id === id;
  const editingSpace = (id: string) => editing.kind === 'space' && editing.id === id;

  // The Space on show is this browser's; the next load opens on it, and
  // this is where that load asks for it back.
  $effect(() => {
    const space = views.sidebar.space;
    if (space) followSpace(space);
  });

  // One row is one element, so a re-listing keeps the DOM it already has --
  // which is what lets the name being typed into a row survive it.
  function key(row: SideRow): string {
    switch (row.kind) {
      case 'space': return `s:${row.id}`;
      case 'thread': return `t:${row.id}`;
      case 'project': return `p:${row.id}`;
      case 'window': return `w:${row.id}`;
      default: return row.kind;
    }
  }

  /** The field is opened to be typed in, over a name that is being replaced. */
  function typeHere(node: HTMLInputElement) {
    node.focus();
    node.select();
  }

  // What a click landed on, as `sidebar::Sidebar::click_target` read it: the
  // nearest element carrying one of the four marks. A click on a row's text
  // or dot is the row; the small buttons and the headers' actions are their
  // own.
  function onClick(ev: MouseEvent) {
    const target = ev.target;
    if (!(target instanceof Element)) return;
    const hit = target.closest('[data-action],[data-thread],[data-window],[data-project]');
    if (!hit) return;
    const thread = hit.getAttribute('data-thread');
    const project = hit.getAttribute('data-project');
    const action = hit.getAttribute('data-action');
    let handled = false;
    switch (action) {
      // The footer's two: the page's own panels, not the model's.
      case 'settings': openSettings(); handled = true; break;
      case 'search': openPalette('threads'); handled = true; break;
      case 'new-thread': handled = sideClick('new-thread', project); break;
      case 'new-project': handled = sideClick('new-project'); break;
      case 'archived': handled = sideClick('toggle-archived'); break;
      case 'pin': handled = sideClick('pin', thread, true); break;
      case 'unpin': handled = sideClick('pin', thread, false); break;
      case 'delete': handled = sideClick('delete', thread); break;
      case 'archive': handled = sideClick('archive', project); break;
      case 'unarchive': handled = sideClick('unarchive', project); break;
      // Anchored under the button, as the view options are: the Spaces
      // are a list to pick from, not a menu for a point.
      case 'space-menu': {
        const box = hit.getBoundingClientRect();
        openMenu('space', '', box.left, box.bottom + 4);
        handled = true;
        break;
      }
      case 'notifications': {
        const box = hit.getBoundingClientRect();
        openMenu('notifications', '', box.left, box.bottom + 4);
        handled = true;
        break;
      }
      // The panel's own: what it shows is the page's, not the server's,
      // so the menu is anchored under the button rather than at a point.
      case 'sidebar-options': {
        const box = hit.getBoundingClientRect();
        openMenu('sidebar-options', '', box.left, box.bottom + 4);
        handled = true;
        break;
      }
      // A single click on a name is the row; renaming is a double click.
      case 'rename-thread': handled = sideClick(ev.detail >= 2 ? 'rename-thread' : 'thread', thread); break;
      case 'rename-project': handled = sideClick(ev.detail >= 2 ? 'rename-project' : 'toggle-project', project); break;
      case null:
        if (thread !== null) handled = sideClick('thread', thread);
        else if (hit.hasAttribute('data-window')) handled = sideClick('window', hit.getAttribute('data-window'));
        else handled = sideClick('toggle-project', project);
        break;
    }
    if (handled) ev.preventDefault();
    // On a phone the panel is a drawer over the canvas: picking a terminal
    // is what it was opened for, so it goes away again.
    if (handled && mobile.on && (thread !== null || hit.hasAttribute('data-window'))) openSide(false);
  }

  // A press on a row is that row's menu: a thread's, a project's, or an
  // archived project's, which is a different list.
  function onMenu(ev: MouseEvent) {
    const target = ev.target;
    if (!(target instanceof Element)) return;
    const row = target.closest('.row.thread[data-thread],.row.project[data-project]');
    if (!row) return;
    ev.preventDefault();
    const thread = row.getAttribute('data-thread');
    if (thread !== null) {
      openMenu('thread', thread, ev.clientX, ev.clientY);
      return;
    }
    const project = row.getAttribute('data-project');
    if (project !== null) {
      openMenu(row.classList.contains('archived') ? 'archived-project' : 'project', project, ev.clientX, ev.clientY);
    }
  }

  // Enter commits the name, Escape drops it; nothing typed in here reaches
  // the terminal's keyboard field.
  function onKeydown(ev: KeyboardEvent) {
    ev.stopPropagation();
    const input = ev.target;
    if (!(input instanceof HTMLInputElement)) return;
    if (ev.key === 'Enter' || ev.key === 'Escape') {
      ev.preventDefault();
      sideKey(ev.key, input.value);
    }
  }

  // The panel's width: a preference of this browser's, within the desktop's
  // bounds (sidebar::MIN_WIDTH..MAX_WIDTH).
  const STORE = 'thinkterm.sidebar';
  const MIN = 127;
  const MAX = 260;
  const DEFAULT = 220;
  let width = DEFAULT;

  function setWidth(px: number): number {
    const w = Math.round(Math.min(MAX, Math.max(MIN, px)));
    document.documentElement.style.setProperty('--side-w', `${w}px`);
    // What the panel is worth when it is put away: `--side-w` goes to 0
    // there, and the hover reveal paints at this instead.
    document.documentElement.style.setProperty('--side-px', `${w}px`);
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


  // The hover reveal: with the panel put away, the window's left edge and
  // the tab row's toggle bring it back as an overlay over the canvas.
  // Nothing is resized -- `--side-w` stays 0 -- so this is the panel's own
  // display state and nothing else's.
  /** How far into the window the edge trigger reaches, in client px. */
  const EDGE = 6;
  const REVEAL_DELAY = 150;
  const RETREAT_DELAY = 250;
  let revealed = $state(false);
  let revealTimer: ReturnType<typeof setTimeout> | null = null;
  let retreatTimer: ReturnType<typeof setTimeout> | null = null;

  /** Something the reveal would land under, or fight with, is up. */
  function suppressed(): boolean {
    return menu.items.length > 0 || palette.open || panel.open || views.sidebar.editing.kind !== 'none';
  }

  function collapsed(): boolean {
    return document.body.dataset.side === 'off';
  }

  function armed(): boolean {
    return collapsed() && views.settings['hover-reveal'] && !suppressed();
  }

  function cancelReveal() {
    if (revealTimer === null) return;
    clearTimeout(revealTimer);
    revealTimer = null;
  }

  function cancelRetreat() {
    if (retreatTimer === null) return;
    clearTimeout(retreatTimer);
    retreatTimer = null;
  }

  function scheduleReveal() {
    cancelRetreat();
    if (revealed || revealTimer !== null || !armed()) return;
    revealTimer = setTimeout(() => {
      revealTimer = null;
      if (armed()) revealed = true;
    }, REVEAL_DELAY);
  }

  function scheduleRetreat() {
    cancelReveal();
    // A menu or a palette opened over the revealed panel keeps it there:
    // the pointer went to them, not away from it.
    if (!revealed || retreatTimer !== null || suppressed()) return;
    retreatTimer = setTimeout(() => {
      retreatTimer = null;
      if (!suppressed()) revealed = false;
    }, RETREAT_DELAY);
  }

  function hide() {
    cancelReveal();
    cancelRetreat();
    revealed = false;
  }

  // One cheap test per pointer move: the two triggers, the two places the
  // pointer may rest, and everything else, which retreats. The panel's own
  // enter/leave below are what keep it up when the pointer is inside it
  // without moving.
  function onWindowPointerMove(ev: PointerEvent) {
    if (!collapsed() || !views.settings['hover-reveal']) {
      if (revealed) hide();
      return;
    }
    const target = ev.target instanceof Element ? ev.target : null;
    if (target?.closest('#side')) {
      cancelRetreat();
      return;
    }
    if (target?.closest('[data-action=sidebar]') || ev.clientX <= EDGE) {
      scheduleReveal();
      return;
    }
    if (revealed) scheduleRetreat();
    else cancelReveal();
  }

  // A press on the toggle pins the panel open the ordinary way; the reveal
  // is then over, whichever way that press went.
  function onWindowClick(ev: MouseEvent) {
    const target = ev.target;
    if (target instanceof Element && target.closest('[data-action=sidebar]')) hide();
  }

  $effect(() => {
    window.addEventListener('pointermove', onWindowPointerMove);
    window.addEventListener('click', onWindowClick);
    return () => {
      window.removeEventListener('pointermove', onWindowPointerMove);
      window.removeEventListener('click', onWindowClick);
      cancelReveal();
      cancelRetreat();
    };
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
    // The terminal is resized from the canvas's box, so a drag that moved it
    // resized the whole grid on every frame -- which is what flickered. Its
    // size is pinned for the drag; only its left edge moves, and the one
    // real resize happens on release.
    const canvas = document.getElementById('term');
    if (canvas instanceof HTMLCanvasElement) {
      const box = canvas.getBoundingClientRect();
      canvas.style.width = `${box.width}px`;
      canvas.style.height = `${box.height}px`;
      frozen = canvas;
    }
  }

  function onPointerMove(ev: PointerEvent) {
    if (dragging) width = setWidth(ev.clientX);
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

<!-- The panel is one delegated handler, as the wasm had it; its children
     are the rows and their buttons, not the container. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<aside
  id="side"
  class:revealed
  class:open={mobile.on && mobile.side}
  data-rows={JSON.stringify(rows)}
  onclick={onClick}
  oncontextmenu={onMenu}
  onkeydown={onKeydown}
  onpointerdown={onPointerDown}
  onpointermove={onPointerMove}
  onpointerup={onPointerUp}
  onpointercancel={onPointerUp}
  onpointerenter={cancelRetreat}
  onpointerleave={scheduleRetreat}
>
  <div class="handle"></div>
  <div class="list">
    {#each rows as row (key(row))}
      {#if row.kind === 'space'}
        <div class="row space">{@html layers}{#if editingSpace(row.id)}<input class="rename" value={row.name} spellcheck="false" use:typeHere>{:else}<span class="t">{row.name}</span>{/if}<span class="act" data-action="space-menu" title={s('web-tip-space')}>{@html ellipsis}</span></div>
      {:else if row.kind === 'new-thread'}
        <div class="top"><div class="pill" data-action="new-thread" title={s('web-tip-new-thread')}>{@html circlePlus}<span>{s('sidebar-new-thread')}</span></div><span class="act round" data-action="notifications" title={s('tooltip-sidebar-notifications')}>{@html bell}</span></div>
      {:else if row.kind === 'pinned'}
        <div class="hdr">{@html pin}<span>{s('sidebar-pinned')}</span></div>
      {:else if row.kind === 'workspaces'}
        <div class="hdr"><span>{s('sidebar-workspaces')}</span><span class="act round" data-action="new-project" title={s('web-tip-new-project')}>{@html folderPlus}</span></div>
        {#if editing.kind === 'new-project'}
          <div class="path"><input placeholder={s('web-path-placeholder')} spellcheck="false" use:typeHere></div>
        {/if}
      {:else if row.kind === 'project' && row.archived}
        <div class="row project archived" data-project={row.id} title={row.path}>{@html archive}<span class="t">{row.name}</span><span class="hov"><span class="act" data-action="unarchive" data-project={row.id} title={s('web-tip-restore')}>{@html archiveRestore}</span></span></div>
      {:else if row.kind === 'project'}
        <div class="row project" data-project={row.id} title={row.path}><span class="chev">{@html row.collapsed ? chevronRight : chevronDown}</span>{@html row.collapsed ? folder : folderOpen}{#if editingProject(row.id)}<input class="rename" value={row.name} spellcheck="false" use:typeHere>{:else}<span class="t" data-action="rename-project" data-project={row.id}>{row.name}</span>{/if}<span class="hov"><span class="act" data-action="archive" data-project={row.id} title={s('web-tip-archive')}>{@html archive}</span></span><span class="act round" data-action="new-thread" data-project={row.id} title={s('web-tip-new-thread-here')}>{@html plus}</span></div>
      {:else if row.kind === 'thread'}
        <div class="row thread" class:selected={row.selected} data-thread={row.id}>{#if row.status === 'Running'}<span class="st spin">{@html loaderCircle}</span>{:else if row.status === 'NeedsAttention'}<span class="st alert">{@html circleAlert}</span>{:else if row.status === 'Done'}<span class="st done">{@html circleCheck}</span>{:else}<span class="st dot {row.dot.toLowerCase()}"></span>{/if}{#if editingThread(row.id)}<input class="rename" value={row.name} spellcheck="false" use:typeHere>{:else}<span class="t" data-action="rename-thread" data-thread={row.id}>{row.name}</span>{/if}<span class="hov"><span
              class="act"
              data-action={row.pinned ? 'unpin' : 'pin'}
              data-thread={row.id}
              title={row.pinned ? s('web-tip-unpin') : s('web-tip-pin')}
            >{@html row.pinned ? pinOff : pin}</span><span
              class="act"
              class:danger={row.deleting}
              data-action="delete"
              data-thread={row.id}
              title={s('web-tip-delete-thread')}
            >{#if row.deleting}{s('web-confirm-delete')}{:else}{@html trash2}{/if}</span></span></div>
      {:else if row.kind === 'archived'}
        <div class="hdr sub" data-action="archived"><span class="chev">{@html row.open ? chevronDown : chevronRight}</span>{@html archive}<span>{row.label}</span></div>
      {:else if row.kind === 'others'}
        <div class="hdr"><span>{s('web-sidebar-other-windows')}</span></div>
      {:else if row.kind === 'window'}
        <div class="row thread" class:selected={row.selected} data-window={row.id}><span class="st dot {row.selected ? 'active' : 'open'}"></span><span class="t">{row.title}</span></div>
      {/if}
    {/each}
  </div>
  <!-- The footer, as the desktop's sidebar ends: Settings on the left,
       the thread search on the right. -->
  <div class="foot">
    <span class="act" data-action="settings" title={s('tooltip-sidebar-settings')}>{@html settingsIcon}</span>
    <span class="act" data-action="search" title={s('tooltip-sidebar-thread-search')}>{@html searchIcon}</span>
  </div>
</aside>
