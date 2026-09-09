<script lang="ts">
  // One bar over each drawn pane, laid out from the canvas's cells: the
  // pane's capsules and the buttons that split, zoom and close it. What
  // `navbar::NavBars::html` drew.
  import { onChromeClick } from './chrome';
  import { s, views } from './client.svelte';
  import { loaderCircle, maximize2, minimize2, plus, squareSplitHorizontal, squareSplitVertical, squareTerminal, x } from './icons';

  const px = (n: number) => n.toFixed(2);
</script>

<!-- The row is one delegated handler, as the wasm had it; its children
     are the controls, not the container. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div id="panes" onclick={onChromeClick}>
  {#each views.navs as bar (bar.rect.pane)}
    <div
      class="nav"
      class:focused={bar.focused}
      data-nav={bar.rect.pane}
      style="left:{px(bar.rect.left)}px;top:{px(bar.rect.top)}px;width:{px(bar.rect.width)}px;height:{px(bar.rect.height)}px"
    ><span class="caps">{#each bar.members as m (m.pane)}<span
          class="cap"
          class:current={m.current}
          data-pane={m.pane}
          title={m.title}
        >{#if m.busy}<span class="spin">{@html loaderCircle}</span>{:else}{@html squareTerminal}{/if}<span class="t">{m.title}</span><span
            class="x"
            class:danger={bar.closing && m.current}
            data-action="close-pane"
            data-pane={m.pane}
            title={s('web-tip-close-pane')}
          >{#if bar.closing && m.current}{s('web-confirm-close')}{:else}{@html x}{/if}</span></span>{/each}</span><span class="acts"><span
          class="act"
          data-action="new-tab"
          title={s('web-tip-new-tab')}>{@html plus}</span><span
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
</div>
