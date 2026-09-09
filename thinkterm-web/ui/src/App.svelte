<script lang="ts">
  // The page's chrome. The sidebar, the tab row, the pane bars, the card
  // and the remark are drawn here from the views the wasm publishes; the
  // canvas and the keyboard field are the wasm's, and it finds them by id.
  import AgentsPanel from './AgentsPanel.svelte';
  import Card from './Card.svelte';
  import ContextMenu from './ContextMenu.svelte';
  import KeyBar from './KeyBar.svelte';
  import NavBars from './NavBars.svelte';
  import SearchPalette from './SearchPalette.svelte';
  import SettingsPanel from './SettingsPanel.svelte';
  import Sidebar from './Sidebar.svelte';
  import Status from './Status.svelte';
  import TabRow from './TabRow.svelte';
  import { views } from './client.svelte';
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
<canvas id="term" data-layout={views.layout} oncontextmenu={onCanvasMenu} use:touch></canvas>
<NavBars />
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
{#if drawn}<div id="scrim" onclick={closeDrawers}></div>{/if}
<Card />
<textarea id="kbd" wrap="off" autocapitalize="off" autocomplete="off" autocorrect="off" spellcheck="false"></textarea>
<Status />
{#if views.settings['agents-panel']}<AgentsPanel />{/if}
{#if mobile.on}<KeyBar />{/if}
<ContextMenu />
<SearchPalette />
<SettingsPanel />
