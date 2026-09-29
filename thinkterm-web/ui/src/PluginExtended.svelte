<script lang="ts">
  // A plugin panel's extended view: the wide area left of the right panel,
  // as the desktop's is left of its sidebar, while the panel's frames ask
  // for one (thinkterm-web/src/app.rs `plugin_panel_extended`). What it
  // shows is PluginPanel's; this is its room -- as wide as it was last
  // dragged for the plugin in this browser, within what the window has --
  // and the handle on its edge that sizes it.
  import PluginPanel from './PluginPanel.svelte';
  import { handle } from './client';
  import { views } from './client.svelte';
  import { EXTENDED_MIN, canExtend, extendedRoom, storeWidth, storedWidth } from './extended';

  let { plugin }: { plugin: string } = $props();

  /** Whether the window has room for it: none, and it is not shown. */
  let room = $state(canExtend());
  /** The width asked for, and the one it has in the room there is. The
      plugin is this one's for good: it is shown anew for another. */
  let wanted = 0;
  let width = 0;

  function setWidth(px: number): number {
    const w = Math.round(Math.min(extendedRoom(), Math.max(EXTENDED_MIN, px)));
    document.documentElement.style.setProperty('--ext-w', `${w}px`);
    handle.client?.resize();
    return w;
  }

  // A window made narrower, or a panel beside it made wider, leaves it what
  // room there is, or none. Its own width is not what it fits to, so
  // fitting it comes to rest.
  $effect(() => {
    const fit = () => {
      room = canExtend();
      if (room && wanted) width = setWidth(wanted);
    };
    window.addEventListener('resize', fit);
    const term = document.getElementById('term');
    const others = new ResizeObserver(fit);
    if (term) others.observe(term);
    return () => {
      window.removeEventListener('resize', fit);
      others.disconnect();
    };
  });

  // The mark on the body is what makes the tab row and the terminal leave
  // room for it, as the right panel's does for the panel.
  $effect(() => {
    if (!room) return;
    wanted ||= storedWidth(plugin);
    document.body.dataset.extended = '';
    width = setWidth(wanted);
    return () => {
      delete document.body.dataset.extended;
      handle.client?.resize();
      handle.client?.plugin_extended_gone();
    };
  });

  let dragging = false;

  function onPointerDown(ev: PointerEvent) {
    const target = ev.target;
    if (!(target instanceof HTMLElement) || !target.classList.contains('handle')) return;
    ev.preventDefault();
    dragging = true;
    handle.client?.panel_drag(true);
    try {
      target.setPointerCapture(ev.pointerId);
    } catch {
      // Without capture the drag still follows the pointer over the area.
    }
  }

  // Its right edge is the right panel's left: its width is what lies
  // between that and the pointer.
  function onPointerMove(ev: PointerEvent) {
    if (!dragging) return;
    // A release the page never saw ends the drag on the next move with no
    // button down.
    if (ev.buttons === 0) {
      onPointerUp();
      return;
    }
    const edge = document.getElementById('agents')?.getBoundingClientRect().left ?? window.innerWidth;
    wanted = edge - ev.clientX;
    width = setWidth(wanted);
  }

  function onPointerUp() {
    if (!dragging) return;
    dragging = false;
    handle.client?.panel_drag(false);
    wanted = width;
    const plugins = views.plugins.state === 'ready' ? views.plugins.rows.map((row) => row.id) : null;
    storeWidth(plugin, width, plugins);
  }
</script>

{#if room}
  <!-- The handle's drag, captured: its moves come to the area. -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <section
    id="extended"
    onpointerdown={onPointerDown}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
    onlostpointercapture={onPointerUp}
  >
    <div class="handle"></div>
    <PluginPanel extended />
  </section>
{/if}
