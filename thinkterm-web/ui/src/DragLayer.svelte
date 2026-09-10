<script lang="ts">
  // What a drag shows: the ghost under the pointer, the 2px line the row or
  // tab would land on, and the half of the pane a capsule would take. The
  // desktop paints the same three (render/paint.rs), in the same accent.
  import { drag, installDrag } from './drag.svelte';
  import { POP, ms } from './motion';
  import { fade } from 'svelte/transition';

  const px = (n: number) => `${n.toFixed(2)}px`;

  function listen(_node: HTMLElement) {
    return { destroy: installDrag() };
  }
</script>

<div id="dnd" use:listen>
  {#if drag.kind}
    <div class="ghost" style="left:{px(drag.x)};top:{px(drag.y)}" transition:fade={{ duration: ms(POP) }}>{drag.label}</div>
  {/if}
  {#if drag.line}
    <div
      class="line"
      style="left:{px(drag.line.left)};top:{px(drag.line.top)};width:{px(drag.line.width)};height:{px(drag.line.height)}"
      transition:fade={{ duration: ms(POP) }}
    ></div>
  {/if}
  {#if drag.zone}
    <div
      class="zone"
      style="left:{px(drag.zone.left)};top:{px(drag.zone.top)};width:{px(drag.zone.width)};height:{px(drag.zone.height)}"
      transition:fade={{ duration: ms(POP) }}
    ></div>
  {/if}
</div>
