<script lang="ts">
  // The page's chrome. The sidebar, the tab row, the pane bars, the card
  // and the remark are drawn here from the views the wasm publishes; the
  // canvas and the keyboard field are the wasm's, and it finds them by id.
  import AgentsPanel from './AgentsPanel.svelte';
  import Boot from './Boot.svelte';
  import Card from './Card.svelte';
  import ContextMenu from './ContextMenu.svelte';
  import DragLayer from './DragLayer.svelte';
  import KeyBar from './KeyBar.svelte';
  import NavBars from './NavBars.svelte';
  import SearchPalette from './SearchPalette.svelte';
  import SettingsPanel from './SettingsPanel.svelte';
  import Sidebar from './Sidebar.svelte';
  import Status from './Status.svelte';
  import TabRow from './TabRow.svelte';
  import { views } from './client.svelte';
  import { HERE, machines } from './machines.svelte';
  import MachinesPanel from './MachinesPanel.svelte';
  import { openMenu } from './menu.svelte';
  import { mobile, openSide } from './mobile.svelte';
  import { paneAt } from './pane';
  import { setSetting } from './settings.svelte';
  import { installSwipe, installTouch } from './touch';

  function onCanvasMenu(ev: MouseEvent) {
    ev.preventDefault();
    const canvas = ev.currentTarget;
    if (!(canvas instanceof HTMLCanvasElement)) return;
    const pane = paneAt(canvas, ev.clientX, ev.clientY);
    if (pane === null) return;
    openMenu('pane', String(pane), ev.clientX, ev.clientY);
  }

  /** Touch on the canvas, and the drawer's swipes with it. Both are inert
      until a finger arrives, so they go up whatever shape the page is in. */
  function touch(node: HTMLCanvasElement) {
    const off = [installTouch(node), installSwipe()];
    return { destroy: () => off.forEach((stop) => stop()) };
  }

  // On a phone the two panels are drawers over the canvas; a press on what
  // is left of the window puts whichever is out away.
  const drawn = $derived(mobile.on && (mobile.side || views.settings['agents-panel']));
  function closeDrawers() {
    openSide(false);
    if (views.settings['agents-panel']) setSetting('agents-panel', false);
  }
</script>

<Sidebar />
<TabRow />
<!-- One canvas and one keyboard field per machine; the one on show is
     #term and #kbd, which is how everything else finds it. -->
<canvas id={machines.active === HERE ? 'term' : 'term-0'} class:off={machines.active !== HERE} data-layout={machines.active === HERE ? views.layout : undefined} oncontextmenu={onCanvasMenu} use:touch></canvas>
{#each machines.open as m (m.n)}
  <canvas id={machines.active === m.id ? 'term' : `term-${m.n}`} class:off={machines.active !== m.id} data-layout={machines.active === m.id ? views.layout : undefined} oncontextmenu={onCanvasMenu} use:touch></canvas>
{/each}
<NavBars />
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
{#if drawn}<div id="scrim" onclick={closeDrawers}></div>{/if}
<Card />
<textarea id={machines.active === HERE ? 'kbd' : 'kbd-0'} class:off={machines.active !== HERE} wrap="off" autocapitalize="off" autocomplete="off" autocorrect="off" spellcheck="false"></textarea>
{#each machines.open as m (m.n)}
  <textarea id={machines.active === m.id ? 'kbd' : `kbd-${m.n}`} class:off={machines.active !== m.id} wrap="off" autocapitalize="off" autocomplete="off" autocorrect="off" spellcheck="false"></textarea>
{/each}
<Status />
<!-- Each machine's own: they follow its client and its #term and #kbd,
     so they come up afresh when another machine comes on show. -->
{#key machines.active}
  {#if views.settings['agents-panel']}<AgentsPanel />{/if}
  {#if mobile.on}<KeyBar />{/if}
{/key}
<ContextMenu />
<DragLayer />
<SearchPalette />
<SettingsPanel />
<MachinesPanel />
<Boot />
