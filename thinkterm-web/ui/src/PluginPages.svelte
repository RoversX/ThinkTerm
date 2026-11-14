<script lang="ts">
  // The plugins' pages on show: one over each drawn pane that holds a page
  // instead of a terminal (thinkterm-web/src/app.rs `layout_json` marks
  // them), in the pane's room under its bar. What each shows is
  // PluginPanel's, opened by the room it is given here and closed when its
  // place goes: with the pane, the tab, or the machine on show.
  import PluginPanel from './PluginPanel.svelte';
  import { handle } from './client';
  import { chromeClick, views } from './client.svelte';
  import { machines } from './machines.svelte';

  type Pane = { id: number; left: number; top: number; cols: number; rows: number; page?: { plugin: string; offset: number } };
  type Layout = { cell?: [number, number]; pad?: [number, number]; focused?: number; panes?: Pane[] };

  const layout = $derived.by((): Layout => {
    try {
      return JSON.parse(views.layout || '{}') as Layout;
    } catch {
      return {};
    }
  });

  /** Each page's room, in the canvas's CSS px. */
  const pages = $derived.by(() => {
    const [cw, ch] = layout.cell ?? [0, 0];
    const [padX, padY] = layout.pad ?? [0, 0];
    if (!(cw > 0) || !(ch > 0)) return [];
    return (layout.panes ?? [])
      .filter((pane) => pane.page)
      .map((pane) => {
        const offset = pane.page?.offset ?? 0;
        return {
          id: pane.id,
          plugin: pane.page?.plugin ?? '',
          left: padX + pane.left * cw,
          top: padY + pane.top * ch + offset,
          width: pane.cols * cw,
          height: Math.max(0, pane.rows * ch - offset),
        };
      });
  });

  const px = (n: number) => n.toFixed(2);

  /** A press in a page that is not the pane in focus makes it so, as a
      press on a terminal does. Before the browser moves its own focus, so a
      press on one of the page's fields still ends in the field. */
  function onPointerDown(pane: number) {
    if (layout.focused !== pane) chromeClick('pane', pane);
  }

  /** Tells the wasm the page's place is gone when its room is: the client
      that had it on show, which is no longer the one on show when another
      machine's has come in its stead. */
  function place(_node: HTMLElement, pane: number) {
    const client = handle.client;
    return {
      destroy() {
        client?.plugin_page_gone(pane);
      },
    };
  }
</script>

<!-- Keyed by the machine on show too: another machine's pane of the same
     number, with a page of the same plugin, is another page, opened afresh
     with that machine's client. -->
{#each pages as page (`${machines.active}:${page.id}:${page.plugin}`)}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="ppage"
    style="left:{px(page.left)}px;top:{px(page.top)}px;width:{px(page.width)}px;height:{px(page.height)}px"
    onpointerdowncapture={() => onPointerDown(page.id)}
    use:place={page.id}
  >
    <PluginPanel page={page.id} />
  </div>
{/each}
