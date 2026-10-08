<script lang="ts">
  // One bar over each drawn pane, laid out from the canvas's cells: the
  // pane's capsules and the buttons that split, zoom and close it. What
  // `navbar::NavBars::html` drew.
  import { onChromeClick } from './chrome';
  import { handle } from './client';
  import { s, views } from './client.svelte';
  import { loaderCircle, maximize2, minimize2, plus, squareSplitHorizontal, squareSplitVertical, squareTerminal, x } from './icons';

  import { armDrag } from './drag.svelte';
  import { iconClass } from './tabicons.svelte';
  import { mobile } from './mobile.svelte';

  type Divider = { row?: number; left?: number; cols?: number; col?: number; top?: number; rows?: number };
  /** Where each divider's handle goes on a phone, in the canvas's CSS px:
      a finger takes hold of a divider anywhere near it (touch.ts), and the
      handle says that it can. */
  const handles = $derived.by(() => {
    if (!mobile.on) return [];
    try {
      const layout = JSON.parse(views.layout || '{}') as { cell?: [number, number]; pad?: [number, number]; dividers?: Divider[] };
      const [cw, ch] = layout.cell ?? [0, 0];
      const [padX, padY] = layout.pad ?? [0, 0];
      if (!(cw > 0) || !(ch > 0)) return [];
      return (layout.dividers ?? []).map((d) =>
        d.row !== undefined
          ? { across: true, x: padX + ((d.left ?? 0) + (d.cols ?? 0) / 2) * cw, y: padY + (d.row + 0.5) * ch, length: (d.cols ?? 0) * cw }
          : { across: false, x: padX + ((d.col ?? 0) + 0.5) * cw, y: padY + ((d.top ?? 0) + (d.rows ?? 0) / 2) * ch, length: (d.rows ?? 0) * ch },
      );
    } catch {
      return [];
    }
  });

  const px = (n: number) => n.toFixed(2);

  /** A finger on a divider's strip takes hold of the divider: the wasm
      drags it from the line's middle, and the rest of the gesture is the
      canvas's, as a mouse's on the line would be. */
  function grab(ev: PointerEvent, h: { x: number; y: number }) {
    const canvas = document.getElementById('term');
    const panes = document.getElementById('panes');
    if (!canvas || !panes) return;
    const box = panes.getBoundingClientRect();
    if (!handle.client?.touch_divider(box.left + h.x, box.top + h.y)) return;
    ev.preventDefault();
    ev.stopPropagation();
    canvas.setPointerCapture(ev.pointerId);
  }

  // A press on a capsule may be its drag onto another pane; the close
  // button is its own, and a press that stays put still focuses the pane.
  function onPointerDown(ev: PointerEvent) {
    const target = ev.target;
    if (!(target instanceof Element) || target.closest('.x')) return;
    const cap = target.closest('.cap');
    const id = cap?.getAttribute('data-pane');
    if (!cap || id === null || id === undefined) return;
    armDrag(ev, { kind: 'pane', id, label: cap.textContent?.trim() ?? id });
  }
</script>

<!-- The row is one delegated handler, as the wasm had it; its children
     are the controls, not the container. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div id="panes" onclick={onChromeClick} onpointerdown={onPointerDown}>
  {#each views.navs as bar (bar.rect.pane)}
    <div
      class="nav"
      class:focused={bar.focused}
      class:lower={bar.rect.top > 0}
      data-nav={bar.rect.pane}
      style="left:{px(bar.rect.left)}px;top:{px(bar.rect.top)}px;width:min({px(bar.rect.width)}px, calc(100% - {px(bar.rect.left)}px));height:{px(bar.rect.height)}px"
    ><span class="caps">{#each bar.members as m (m.pane)}<span
          class="cap"
          class:current={m.current}
          data-pane={m.pane}
          title={m.title}
        >{#if iconClass(m.icon)}<span class="ticon {iconClass(m.icon)}">{#if m.busy}<span class="spin">{@html loaderCircle}</span>{:else}<i></i>{/if}</span>{:else if m.busy}<span class="spin">{@html loaderCircle}</span>{:else}{@html squareTerminal}{/if}<span class="t">{m.title}</span><span
            class="x"
            class:danger={bar.closing && m.current}
            data-action="close-pane"
            data-pane={m.pane}
            title={s('web-tip-close-pane')}
          >{#if bar.closing && m.current}{s('web-confirm-close')}{:else}{@html x}{/if}</span></span>{/each}</span><span class="acts"><span
          class="act"
          data-action="new-pane"
          title={s('web-tip-new-pane')}>{@html plus}</span><span
          class="act"
          data-action="split-below"
          title={s('web-tip-split-down')}>{@html squareSplitVertical}</span><span
          class="act"
          data-action="split-right"
          title={s('web-tip-split-right')}>{@html squareSplitHorizontal}</span><span
          class="act"
          data-action="zoom"
          title={bar.zoomed ? s('web-tip-unzoom') : s('web-tip-zoom')}
        >{#if bar.zoomed}{@html minimize2}{:else}{@html maximize2}{/if}</span></span></div>
  {/each}
  {#each handles as h, i (i)}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <span class="grip" class:across={h.across} style="left:{px(h.x)}px;top:{px(h.y)}px;{h.across ? 'width' : 'height'}:{px(h.length)}px" onpointerdown={(ev) => grab(ev, h)}></span>
  {/each}
</div>
