<script lang="ts">
  // The desktop's left panel: Spaces, Projects and Threads, plus the
  // windows that belong to no thread. What `sidebar::html` drew, from the
  // rows the wasm publishes; the clicks and the inline name go back to it.
  import { s, views, sideClick, sideKey } from './client.svelte';
  import {
    archive, archiveRestore, chevronDown, chevronRight, circleAlert, circleCheck, circlePlus,
    bell, ellipsis, folder, folderOpen, folderPlus, layers, loaderCircle, panelLeft, pin, pinOff, plus,
    trash2,
  } from './icons';
  import { menu, openMenu } from './menu.svelte';
  import { armDrag } from './drag.svelte';
  import { handle } from './client';
  import { toggleSidebar } from './chrome';
  import { iconByName } from './icons';
  import { openPalette } from './palette.svelte';
  import { openSettings } from './settings.svelte';
  import { mobile, openSide } from './mobile.svelte';
  import { palette } from './palette.svelte';
  import { followSpace, panel } from './settings.svelte';
  import type { FooterAction, SideRow } from './model';

  const rows = $derived(views.sidebar.rows);
  const footer = $derived(views.sidebar.footer);
  /** The panel is narrower than the width the model says the footer's
      label needs. Measured, because a container query cannot read a
      value the model supplies. */
  let narrow = $state(false);
  function watchWidth(node: HTMLElement) {
    const limit = () => views.sidebar.footer_label_min_width;
    const observer = new ResizeObserver(([entry]) => {
      narrow = entry.contentRect.width < limit();
    });
    observer.observe(node);
    return { destroy: () => observer.disconnect() };
  }
  const editing = $derived(views.sidebar.editing);
  const addingWorkspace = $derived(editing.kind === 'new-project');
  const addError = $derived(views.sidebar.new_project_error);
  /** Nothing to work in yet: the tree's own invitation to add one. */
  const noWorkspaces = $derived(!rows.some((r) => r.kind === 'project'));
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

  // A refused path keeps the field, so nothing is retyped; the caret goes
  // back to it with the whole path selected, ready to be replaced.
  $effect(() => {
    if (!addError) return;
    const input = document.querySelector<HTMLInputElement>('#side .path input');
    if (!input) return;
    input.focus();
    input.select();
  });

  // The thread on show is kept in view: the one a just-added workspace
  // brought with it is at the end of a list that may have scrolled away.
  // `nearest` scrolls only when it has to, so a thread already on screen
  // is left where the reader put it.
  let shown = '';
  $effect(() => {
    const row = rows.find((r) => r.kind === 'thread' && r.selected);
    const id = row && row.kind === 'thread' ? row.id : '';
    if (id === '' || id === shown) return;
    shown = id;
    const el = document.querySelector(`#side .row.thread[data-thread="${CSS.escape(id)}"]`);
    el?.scrollIntoView({ block: 'nearest' });
  });

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
      case 'sidebar': toggleSidebar(); handled = true; break;
      case 'archived': handled = sideClick('toggle-archived'); break;
      case 'pin': handled = sideClick('pin', thread, true); break;
      case 'unpin': handled = sideClick('pin', thread, false); break;
      case 'delete': handled = sideClick('delete', thread); break;
      case 'archive': handled = sideClick('archive', project); break;
      case 'unarchive': handled = sideClick('unarchive', project); break;
      // The whole row is the Space button, as on the desktop; the list
      // hangs under it.
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
  // How far the edge trigger reaches and how long the two dwells are is
  // the model's (thinkterm-web/src/sidebar.rs `REVEAL`), not this layer's.
  const reveal = $derived(views.sidebar.reveal);
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
    }, reveal.dwell_ms);
  }

  function scheduleRetreat() {
    cancelReveal();
    // A menu or a palette opened over the revealed panel keeps it there:
    // the pointer went to them, not away from it.
    if (!revealed || retreatTimer !== null || suppressed()) return;
    retreatTimer = setTimeout(() => {
      retreatTimer = null;
      if (!suppressed()) revealed = false;
    }, reveal.retreat_ms);
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
    if (target?.closest('[data-action=sidebar]') || ev.clientX <= reveal.edge) {
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
    window.addEventListener('blur', onPointerUp);
    return () => {
      window.removeEventListener('pointermove', onWindowPointerMove);
      window.removeEventListener('click', onWindowClick);
      window.removeEventListener('blur', onPointerUp);
      cancelReveal();
      cancelRetreat();
    };
  });

  let dragging = false;
  /** The canvas whose box is held still for the drag, so it resizes once. */

  // A press on a thread or project row may be its drag. Pinned threads and
  // archived projects are neither dragged nor landed on -- the model orders
  // them elsewhere -- and a press on a button, or in the name being typed,
  // is not a row's at all.
  function armRowDrag(ev: PointerEvent, target: Element) {
    if (target.closest('.act,.hov,input')) return;
    const el = target.closest('.row.thread[data-thread],.row.project[data-project]');
    if (!el) return;
    const thread = el.getAttribute('data-thread');
    const project = el.getAttribute('data-project');
    const row = views.sidebar.rows.find((r) =>
      thread !== null ? r.kind === 'thread' && r.id === thread : r.kind === 'project' && r.id === project);
    if (row?.kind === 'thread' && !row.pinned) {
      armDrag(ev, { kind: 'thread', id: row.id, label: row.name, project: row.project });
    } else if (row?.kind === 'project' && !row.archived) {
      armDrag(ev, { kind: 'project', id: row.id, label: row.name });
    }
  }

  function onPointerDown(ev: PointerEvent) {
    const target = ev.target;
    if (!(target instanceof HTMLElement)) return;
    if (!target.classList.contains('handle')) {
      armRowDrag(ev, target);
      return;
    }
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

  function onPointerMove(ev: PointerEvent) {
    if (!dragging) return;
    // A release the page never saw (over another window, say) ends the
    // drag on the next move with no button down.
    if (ev.buttons === 0) {
      onPointerUp();
      return;
    }
    width = setWidth(ev.clientX);
    // Refit in this same task: the observer would run a frame later,
    // with the old bitmap stretched across the new box meanwhile.
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

{#snippet footAction(action: FooterAction)}
  <span
    class="act"
    class:lbl={action.label !== null}
    class:off={!action.enabled}
    data-action={action.enabled ? action.id : undefined}
    aria-disabled={!action.enabled}
    title={action.tip}
  >{@html iconByName(action.icon) ?? ''}{#if action.label !== null}<span class="tl">{action.label}</span>{/if}</span>
{/snippet}

<!-- The panel is one delegated handler, as the wasm had it; its children
     are the rows and their buttons, not the container. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<aside
  id="side"
  use:watchWidth
  class:narrow
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
  onlostpointercapture={onPointerUp}
  onpointerenter={cancelRetreat}
  onpointerleave={scheduleRetreat}
>
  <div class="handle"></div>
  <div class="bar"><span class="act" data-action="sidebar" title={s('web-tip-sidebar')}>{@html panelLeft}</span></div>
  <div class="list">
    {#each rows as row (key(row))}
      {#if row.kind === 'space'}
        <div class="row space" data-action={editingSpace(row.id) ? undefined : 'space-menu'} title={s('web-tip-space')}>{@html layers}{#if editingSpace(row.id)}<input class="rename" value={row.name} spellcheck="false" use:typeHere>{:else}<span class="t">{row.name}</span>{/if}<span class="act">{@html ellipsis}</span></div>
      {:else if row.kind === 'new-thread'}
        <div class="top"><div class="pill" data-action="new-thread" title={s('web-tip-new-thread')}>{@html circlePlus}<span>{s('sidebar-new-thread')}</span></div><span class="act round" data-action="notifications" title={s('tooltip-sidebar-notifications')}>{@html bell}</span></div>
      {:else if row.kind === 'pinned'}
        <div class="hdr">{@html pin}<span>{s('sidebar-pinned')}</span></div>
      {:else if row.kind === 'workspaces'}
        <div class="hdr"><span>{s('sidebar-workspaces')}</span><span class="act round" data-action="new-project" title={s('web-tip-new-project')}>{@html folderPlus}</span></div>
        {#if addingWorkspace}
          <div class="path" class:bad={!!addError}>
            <div class="lb">{@html folderPlus}<span>{s('web-add-workspace')}</span></div>
            <input
              placeholder={s('web-path-placeholder')}
              spellcheck="false"
              autocapitalize="off"
              autocorrect="off"
              enterkeyhint="go"
              use:typeHere
            >
            <div class="hint">{addError ?? s('web-add-workspace-hint')}</div>
          </div>
        {:else if noWorkspaces}
          <div class="hdr add" data-action="new-project" title={s('web-tip-new-project')}>{@html folderPlus}<span>{s('web-add-workspace')}</span></div>
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
  <!-- The footer, as the desktop's sidebar ends: which actions it carries,
       what they are called, which are refused and which sit at the right
       inset are the model's (app.rs `side_footer`); the label goes when
       the panel is narrower than the model's own threshold. -->
  <div class="foot">
    {#each footer.filter((a) => !a.trailing) as action (action.id)}
      {@render footAction(action)}
    {/each}
    <span class="acts">
      {#each footer.filter((a) => a.trailing) as action (action.id)}
        {@render footAction(action)}
      {/each}
    </span>
  </div>
</aside>
