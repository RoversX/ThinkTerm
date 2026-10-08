<script lang="ts">
  // The window's tab row: the sidebar toggle, a capsule per tab of this
  // window, the Remote Hosts tab when it is open, the new-tab button and the
  // right panel's toggle. What `chrome::TabStrip::render` drew.
  import { onChromeClick } from './chrome';
  import { s, views } from './client.svelte';
  import { link2, panelLeft, panelRight, plus, squareTerminal, x } from './icons';
  import { closePanel, hidePanel, machines, openPanel } from './machines.svelte';
  import { iconClass } from './tabicons.svelte';
  import { openMenu } from './menu.svelte';
  import { armDrag } from './drag.svelte';
  import { short } from './title';

  const tabs = $derived(views.tabs?.tabs ?? []);
  const controls = $derived(views.tabs?.controls ?? null);
  /** The card heading every tab, the terminal's; none with icons off. */
  const icon = $derived(iconClass(views.tabs?.icon));

  // The Remote Hosts tab is the page's own, not the wasm's: its capsule
  // puts the page on show and its close button closes it. Any other tab --
  // or a new one -- puts the page behind the terminal and keeps its tab, as
  // the desktop's tab row does.
  function onClick(ev: MouseEvent) {
    const target = ev.target;
    if (target instanceof Element) {
      if (target.closest('[data-page-tab]')) {
        if (target.closest('.x')) closePanel();
        else if (!machines.panel) openPanel();
        ev.preventDefault();
        return;
      }
      if (target.closest('.tab, [data-action=new-tab]') && !target.closest('.x')) hidePanel();
    }
    onChromeClick(ev);
  }

  // A press on a capsule -- its close button included -- is that tab's.
  // The Remote Hosts tab has none.
  function onMenu(ev: MouseEvent) {
    const target = ev.target;
    if (!(target instanceof Element)) return;
    if (target.closest('[data-page-tab]')) {
      ev.preventDefault();
      return;
    }
    const tab = target.closest('.tab')?.getAttribute('data-tab');
    if (tab === null || tab === undefined) return;
    ev.preventDefault();
    openMenu('tab', tab, ev.clientX, ev.clientY);
  }

  // A press on a capsule may be its drag; the close button is its own, and
  // a press that never travels is still the click that switches tab.
  function onPointerDown(ev: PointerEvent) {
    const target = ev.target;
    if (!(target instanceof Element) || target.closest('.x')) return;
    const cap = target.closest('.tab');
    const id = cap?.getAttribute('data-tab');
    if (!cap || id === null || id === undefined) return;
    armDrag(ev, { kind: 'tab', id, label: cap.textContent?.trim() ?? id });
  }
</script>

<!-- The row is one delegated handler, as the wasm had it; its children
     are the controls, not the container. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div id="tabs" onclick={onClick} oncontextmenu={onMenu} onpointerdown={onPointerDown}>
  <span class="lead"><span class="act" data-action="sidebar" title={s('web-tip-sidebar')}>{@html panelLeft}</span></span>
  <!-- What scrolls on a phone, under the buttons at either end, which stay
       put; elsewhere it is no box of its own, and the row scrolls. -->
  <span class="strip">
  {#each tabs as tab (tab.tab)}
    <span class="tab" class:current={tab.current && !machines.panel} data-pane={tab.target} data-tab={tab.tab} title={tab.title}>
      {#if icon}<span class="ticon {icon}"><i></i></span>{:else}{@html squareTerminal}{/if}<span class="t">{short(tab.label)}</span><span
        class="x"
        class:danger={controls?.closing === tab.tab}
        data-action="close-tab"
        data-tab={tab.tab}
        title={s('web-tip-close-tab')}
      >{#if controls?.closing === tab.tab}{s('web-confirm-close')}{:else}{@html x}{/if}</span>
    </span>
  {/each}
  <!-- After the terminal's tabs, as the desktop places its Remote Hosts
       page's: a link glyph, the title, and a close button. -->
  {#if machines.tab}
    <span class="tab" class:current={machines.panel} data-page-tab="machines" title={s('web-machines-title')}>
      {@html link2}<span class="t">{s('web-machines-title')}</span><span class="x" title={s('web-tip-close-tab')}>{@html x}</span>
    </span>
  {/if}
  <span class="act" data-action="new-tab" title={s('web-tip-new-tab')}>{@html plus}</span>
  </span>
  <!-- The row's right end, as the desktop's tab bar has it: the right
       panel's toggle. -->
  <span class="trail">
    <span
      class="act"
      class:on={views.settings['agents-panel']}
      data-action="agents"
      title={s('web-tip-agents')}>{@html panelRight}</span>
  </span>
</div>
