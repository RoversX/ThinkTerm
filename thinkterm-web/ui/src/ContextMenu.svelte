<script lang="ts">
  // The one open context menu, as a panel per open submenu. What each menu
  // offers and what a row does are the wasm's (thinkterm-web/src/menu.rs);
  // drawn here, clamped into the viewport, walked with the pointer or the
  // keyboard, and closed by anything that means the page moved on.
  import { closeInnermost, closeMenu, menu, menuTrigger, runItem } from './menu.svelte';
  import { chevronRight, iconByName } from './icons';
  import { mobile } from './mobile.svelte';
  import type { MenuItem } from './model';

  /** How near an edge a panel may come before it flips. */
  const EDGE = 6;

  /** The edges a panel must stay inside. On a phone that is the visual
      viewport less the key bar -- the soft keyboard shrinks the former and
      the bar sits at its foot -- not the layout viewport, which is the whole
      window whatever is drawn over its lower half. */
  function bounds(): { right: number; bottom: number } {
    const vv = window.visualViewport;
    if (!mobile.on || !vv) {
      return { right: window.innerWidth - EDGE, bottom: window.innerHeight - EDGE };
    }
    const keybar = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--keybar')) || 0;
    return { right: vv.offsetLeft + vv.width - EDGE, bottom: vv.offsetTop + vv.height - keybar - EDGE };
  }
  /** How long the pointer rests on a submenu row before it opens. */
  const HOVER = 150;

  // The panels on show: the root list, then the submenu each open row has.
  const panels = $derived.by(() => {
    const out: MenuItem[][] = [];
    if (menu.items.length === 0) return out;
    let items = menu.items;
    out.push(items);
    for (const i of menu.path) {
      const sub = items[i]?.submenu;
      if (!sub || sub.length === 0) break;
      out.push(sub);
      items = sub;
    }
    return out;
  });

  const innermost = $derived(panels[panels.length - 1] ?? []);
  const selectable = (item: MenuItem | undefined) => !!item && item.kind === 'item' && item.enabled;

  let boxes = $state<(HTMLDivElement | null)[]>([]);
  let at = $state<{ x: number; y: number }[]>([]);

  // Measured, not guessed: a panel's size is its rows' text, and the point
  // it was asked for can be a few pixels from the edge of the window.
  $effect(() => {
    const panelCount = panels.length;
    const anchor = [menu.x, menu.y];
    const next: { x: number; y: number }[] = [];
    for (let k = 0; k < panelCount; k++) {
      const el = boxes[k];
      if (!el) {
        next.push({ x: anchor[0], y: anchor[1] });
        continue;
      }
      const w = el.offsetWidth;
      const h = el.offsetHeight;
      const { right, bottom } = bounds();
      let x: number;
      let y: number;
      if (k === 0) {
        x = anchor[0];
        y = anchor[1];
        if (x + w > right) x = Math.max(EDGE, anchor[0] - w);
        if (y + h > bottom) y = Math.max(EDGE, anchor[1] - h);
      } else {
        // Beside the row that opened it, overlapping the panel's padding
        // so the pointer crosses no gap on its way in.
        const parent = boxes[k - 1];
        const box = parent?.getBoundingClientRect();
        const row = parent?.querySelector(`[data-idx="${menu.path[k - 1]}"]`)?.getBoundingClientRect();
        x = box ? box.right - 4 : anchor[0];
        y = row ? row.top - 5 : (box?.top ?? anchor[1]);
        if (x + w > right && box) x = Math.max(EDGE, box.left - w + 4);
        if (y + h > bottom) y = Math.max(EDGE, bottom - h);
      }
      next.push({ x, y });
    }
    at = next;
  });

  // The menu takes the keyboard while it is open, which means #kbd loses
  // it; closeMenu hands it back.
  $effect(() => {
    const el = boxes[panels.length - 1];
    if (el && document.activeElement !== el) el.focus({ preventScroll: true });
  });

  // Anything that means the page moved on closes the menu. The press is
  // watched in the capture phase so a click meant for what is underneath
  // does not also act on it.
  $effect(() => {
    if (menu.items.length === 0) return;
    const inside = (target: EventTarget | null) =>
      target instanceof Node && boxes.some((b) => !!b && b.contains(target));
    // The button the menu came from closes it itself, on its click:
    // closing here as well had the click open it again, a flash.
    const down = (ev: Event) => {
      const from = menuTrigger();
      if (from && ev.target instanceof Node && from.contains(ev.target)) return;
      if (!inside(ev.target)) closeMenu();
    };
    const away = () => closeMenu();
    // A wheel is the page moving on only outside the menu, and only once
    // the menu has been up a moment: a trackpad's scroll coasts on after
    // the fingers lift, and a menu opened just after scrolling the sidebar
    // used to close itself on the coast.
    const opened = performance.now();
    const wheel = (ev: Event) => {
      if (inside(ev.target) || performance.now() - opened < 400) return;
      closeMenu();
    };
    // On a phone a resize is the soft keyboard coming or going, which is
    // not the page moving on; the menu that opened the keyboard's owner
    // must not vanish because of it.
    const resized = () => {
      if (!mobile.on) closeMenu();
    };
    window.addEventListener('pointerdown', down, true);
    window.addEventListener('blur', away);
    window.addEventListener('resize', resized);
    window.addEventListener('wheel', wheel, { capture: true, passive: true });
    return () => {
      window.removeEventListener('pointerdown', down, true);
      window.removeEventListener('blur', away);
      window.removeEventListener('resize', resized);
      window.removeEventListener('wheel', wheel, { capture: true });
    };
  });

  let timer: ReturnType<typeof setTimeout> | null = null;
  function stopTimer() {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  }

  /** Open the submenu of row `index` in the innermost panel. */
  function openSub(index: number): boolean {
    const item = innermost[index];
    if (!selectable(item) || item.submenu.length === 0) return false;
    menu.path = [...menu.path, index];
    menu.selected = item.submenu.findIndex(selectable);
    return true;
  }

  /** The keyboard's row, `delta` selectable rows on; headers, separators
      and disabled rows are passed over. */
  function move(delta: number) {
    const items = innermost;
    let i = menu.selected;
    for (let n = 0; n < items.length; n++) {
      i = i < 0 ? (delta > 0 ? 0 : items.length - 1) : (i + delta + items.length) % items.length;
      if (selectable(items[i])) {
        menu.selected = i;
        return;
      }
    }
  }

  function onKeydown(ev: KeyboardEvent) {
    switch (ev.key) {
      case 'ArrowDown': move(1); break;
      case 'ArrowUp': move(-1); break;
      case 'ArrowRight':
        if (menu.selected >= 0) openSub(menu.selected);
        break;
      case 'ArrowLeft':
      case 'Escape':
        stopTimer();
        closeInnermost();
        break;
      case 'Enter': {
        const item = innermost[menu.selected];
        if (!item) break;
        if (item.submenu.length > 0) openSub(menu.selected);
        else runItem(item);
        break;
      }
      default:
        return;
    }
    // Nothing typed at a menu reaches the terminal.
    ev.preventDefault();
    ev.stopPropagation();
  }

  function onEnterRow(panel: number, index: number) {
    stopTimer();
    const item = panels[panel]?.[index];
    if (!selectable(item)) return;
    const already = menu.path.length > panel && menu.path[panel] === index;
    if (already) return;
    // The pointer chose this panel: whatever was open deeper is stale.
    if (menu.path.length > panel) menu.path = menu.path.slice(0, panel);
    menu.selected = index;
    if (item.submenu.length === 0) return;
    timer = setTimeout(() => {
      timer = null;
      menu.path = [...menu.path.slice(0, panel), index];
      menu.selected = item.submenu.findIndex(selectable);
    }, HOVER);
  }

  function onClickRow(panel: number, index: number) {
    stopTimer();
    const item = panels[panel]?.[index];
    if (!selectable(item)) return;
    if (item.submenu.length > 0) {
      menu.path = [...menu.path.slice(0, panel), index];
      menu.selected = item.submenu.findIndex(selectable);
      return;
    }
    runItem(item);
  }
</script>

<!-- The rows are the menu's, delegated per panel; a row is not a button
     and never takes focus, which stays on the panel for the keyboard. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
{#each panels as items, k (k)}
  <div
    id={k === 0 ? 'menu' : undefined}
    class="menu"
    role="menu"
    tabindex="-1"
    bind:this={boxes[k]}
    style="left:{(at[k]?.x ?? menu.x).toFixed(2)}px;top:{(at[k]?.y ?? menu.y).toFixed(2)}px"
    onkeydown={onKeydown}
    onpointerleave={stopTimer}
  >
    {#each items as item, i (i)}
      {#if item.kind === 'separator'}
        <hr class="ms" />
      {:else if item.kind === 'header'}
        <div class="mh">{item.label}</div>
      {:else}
        <div
          class="mi"
          class:checked={item.checked}
          class:disabled={!item.enabled}
          class:on={k === panels.length - 1 && menu.selected === i}
          role="menuitem"
          tabindex="-1"
          aria-disabled={!item.enabled}
          data-id={item.id}
          data-idx={i}
          onclick={() => onClickRow(k, i)}
          onpointerenter={() => onEnterRow(k, i)}
        >
          <span class="ic">{#if item.icon}{@html iconByName(item.icon) ?? ''}{/if}</span><span
            class="lb">{item.label}</span
          >{#if item.submenu.length > 0}<span class="sub">{@html chevronRight}</span>{/if}
        </div>
      {/if}
    {/each}
  </div>
{/each}
