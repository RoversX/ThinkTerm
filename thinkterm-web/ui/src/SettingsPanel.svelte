<script lang="ts">
  // The settings panel: the page's own preferences, one row each. The wasm
  // holds them (thinkterm-web/src/settings.rs) and applies the language and
  // the font itself; the theme is painted here, and the rest is read by
  // whatever it concerns.
  import { s, views } from './client.svelte';
  import { bot, info, minus, palette, panelLeft, plus, rotateCcw, search, slidersHorizontal, x } from './icons';
  import { applyTheme, closeSettings, panel, setSetting } from './settings.svelte';
  import type { Hotkey, Theme } from './model';

  const settings = $derived(views.settings);

  // The window's sections, as the desktop's settings window lists them
  // on the left; the page has fewer rows, so fewer sections.
  type Section = 'general' | 'appearance' | 'sidebar' | 'agents' | 'about';
  const SECTIONS: [Section, string, string][] = [
    ['general', 'settings-section-general', slidersHorizontal],
    ['appearance', 'settings-section-appearance', palette],
    ['sidebar', 'settings-section-sidebar', panelLeft],
    ['agents', 'settings-section-agents', bot],
    ['about', 'settings-section-about', info],
  ];
  let section = $state<Section>('general');
  let query = $state('');

  /** A description: its catalogue text, or nothing while the key is missing. */
  const d = (id: string) => (views.strings[id] === undefined ? '' : views.strings[id]);

  /** Whether a row is on show: its section's, or anywhere when searching. */
  function shows(where: Section, label: string): boolean {
    const q = query.trim().toLowerCase();
    if (q !== '') return label.toLowerCase().includes(q);
    return section === where;
  }

  function step(by: number) {
    pin(Math.round((pt + by) * 2) / 2);
  }
  const font = $derived(settings.font);
  const pinned = $derived(font.mode === 'pinned');

  const THEMES: [Theme, string][] = [
    ['dark', 'web-theme-dark'],
    ['light', 'web-theme-light'],
    ['system', 'web-theme-system'],
  ];
  // The shortcuts as they are pressed, not as they are stored.
  const HOTKEYS: [Hotkey, string][] = [
    ['cmd-k', '⌘K'],
    ['cmd-shift-p', '⌘⇧P'],
    ['ctrl-shift-p', 'Ctrl+Shift+P'],
  ];

  // The theme is the page's to paint, whether the panel is open or not.
  $effect(() => applyTheme(settings.theme));

  // The size the "Fixed size" field offers: the pinned one, else the size
  // the terminal is drawn at now, so choosing "Fixed size" pins what is
  // already on screen rather than a number out of nowhere.
  let pt = $state(12);
  $effect(() => {
    if (!panel.open) return;
    const current = settings.font;
    if (current.mode === 'pinned') {
      pt = current.pt;
      return;
    }
    try {
      const layout = JSON.parse((document.getElementById('term') as HTMLCanvasElement | null)?.dataset.layout || '{}');
      if (Number.isFinite(layout.font_pt) && layout.font_pt > 0) pt = layout.font_pt;
    } catch {
      // Before the first frame there is no layout; the default stands.
    }
  });

  function pin(size: number) {
    pt = Math.min(72, Math.max(6, Number.isFinite(size) ? size : 12));
    setSetting('font', { mode: 'pinned', pt });
  }

  function onSize(ev: Event) {
    const field = ev.currentTarget;
    if (field instanceof HTMLInputElement) pin(Number(field.value));
  }

  function onLanguage(ev: Event) {
    const field = ev.currentTarget;
    if (field instanceof HTMLSelectElement) setSetting('language', field.value);
  }

  function onHotkey(ev: Event) {
    const field = ev.currentTarget;
    if (field instanceof HTMLSelectElement) setSetting('palette-hotkey', field.value);
  }

  function onFlag(key: string, ev: Event) {
    const field = ev.currentTarget;
    if (field instanceof HTMLInputElement) setSetting(key, field.checked);
  }

  /** The sidebar's width is this browser's, kept under the Sidebar's own
      key; the reset puts the panel and the key back to the default. */
  function resetSidebar() {
    document.documentElement.style.setProperty('--side-w', '220px');
    document.documentElement.style.setProperty('--side-px', '220px');
    try {
      localStorage.setItem('thinkterm.sidebar', '220');
    } catch {
      // A browser that blocks storage keeps it for this load only.
    }
    setSetting('sidebar-width', 220);
  }

  // The locale the language preference actually came to, and where the
  // page is served from. Re-read when the language changes.
  const about = $derived.by(() => {
    void settings.language;
    const locale = document.documentElement.lang;
    return locale ? `${locale} · ${location.host}` : location.host;
  });

  // The panel takes the keyboard while it is up, so nothing typed into it
  // reaches the terminal; closing hands focus back.
  function onKeydown(ev: KeyboardEvent) {
    ev.stopPropagation();
    if (ev.key !== 'Escape') return;
    ev.preventDefault();
    closeSettings();
  }

  function focusHere(node: HTMLDivElement) {
    node.focus({ preventScroll: true });
  }
</script>

<!-- The desktop's settings window: sections on the left, the section's
     cards on the right, a search field that finds a row wherever it is. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
{#if panel.open}
  <div id="settings-back" onpointerdown={closeSettings}></div>
  <div id="settings" role="dialog" aria-modal="true" tabindex="-1" onkeydown={onKeydown} use:focusHere>
    <button class="sc" type="button" title={s('web-settings-close')} onclick={closeSettings}>{@html x}</button>
    <div class="snav">
      <div class="app">ThinkTerm</div>
      <label class="find">{@html search}<input placeholder={s('web-settings-search')} bind:value={query} spellcheck="false" /></label>
      <div class="secs">
        {#each SECTIONS as [id, label, icon] (id)}
          <div class="sec" class:on={query.trim() === '' && section === id} data-section={id} onclick={() => { section = id; query = ''; }}>{@html icon}<span>{s(label)}</span></div>
        {/each}
      </div>
    </div>
    <div class="body">
      <div class="title">{query.trim() === '' ? s(SECTIONS.find(([id]) => id === section)?.[1] ?? '') : s('web-settings-search')}</div>

      {#if shows('general', s('settings-language')) || shows('general', s('web-settings-palette-hotkey'))}
        <div class="card">
          {#if shows('general', s('settings-language'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('settings-language')}</div><div class="desc">{d('web-settings-language-description')}</div></div>
              <select class="pillsel" data-setting="language" value={settings.language} onchange={onLanguage}>
                {#each panel.languages as option (option.preference)}
                  <option value={option.preference}>{option.label}</option>
                {:else}
                  <option value="system">{s('language-system')}</option>
                {/each}
              </select>
            </div>
          {/if}
          {#if shows('general', s('web-settings-palette-hotkey'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('web-settings-palette-hotkey')}</div><div class="desc">{d('web-settings-hotkey-description')}</div></div>
              <select class="pillsel" data-setting="palette-hotkey" value={settings['palette-hotkey']} onchange={onHotkey}>
                {#each HOTKEYS as [value, label] (value)}
                  <option value={value}>{label}</option>
                {/each}
              </select>
            </div>
          {/if}
        </div>
      {/if}

      {#if shows('appearance', s('web-settings-theme')) || shows('appearance', s('web-settings-font'))}
        <div class="card">
          {#if shows('appearance', s('web-settings-theme'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('web-settings-theme')}</div><div class="desc">{d('web-settings-theme-description')}</div></div>
              <select class="pillsel" data-setting="theme" value={settings.theme} onchange={(ev) => setSetting('theme', (ev.currentTarget as HTMLSelectElement).value)}>
                {#each THEMES as [value, label] (value)}
                  <option value={value}>{s(label)}</option>
                {/each}
              </select>
            </div>
          {/if}
          {#if shows('appearance', s('web-settings-font'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('web-settings-font')}</div><div class="desc">{d('web-settings-font-description')}</div></div>
              <div class="ctl">
                <span class="seg">
                  <label class="sg" class:on={!pinned}><input type="radio" name="font" data-font="follow" checked={!pinned} onchange={() => setSetting('font', { mode: 'follow' })} />{s('web-font-follow')}</label>
                  <label class="sg" class:on={pinned}><input type="radio" name="font" data-font="pinned" checked={pinned} onchange={() => pin(pt)} />{s('web-font-pinned')}</label>
                </span>
                <button class="reset" type="button" title={s('web-cmd-font-reset')} disabled={!pinned} onclick={() => pin(12)}>{@html rotateCcw}</button>
                <span class="stepper" class:off={!pinned}>
                  <button type="button" disabled={!pinned} onclick={() => step(-0.5)}>{@html minus}</button>
                  <input class="num" type="number" min="6" max="72" step="0.5" value={pt} disabled={!pinned} onchange={onSize} />
                  <button type="button" disabled={!pinned} onclick={() => step(0.5)}>{@html plus}</button>
                </span>
              </div>
            </div>
          {/if}
        </div>
      {/if}

      {#if shows('sidebar', s('web-settings-hover-reveal')) || shows('sidebar', s('web-settings-sidebar-reset'))}
        <div class="card">
          {#if shows('sidebar', s('web-settings-hover-reveal'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('web-settings-hover-reveal')}</div><div class="desc">{d('web-settings-hover-reveal-description')}</div></div>
              <label class="switch"><input type="checkbox" data-setting="hover-reveal" checked={settings['hover-reveal']} onchange={(ev) => onFlag('hover-reveal', ev)} /><span class="knob"></span></label>
            </div>
          {/if}
          {#if shows('sidebar', s('web-settings-sidebar-reset'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('web-settings-sidebar-reset')}</div><div class="desc">{d('web-settings-sidebar-reset-description')}</div></div>
              <button class="pillbtn" type="button" data-action="reset-sidebar" onclick={resetSidebar}>{s('common-reset')}</button>
            </div>
          {/if}
        </div>
      {/if}

      {#if shows('agents', s('web-settings-agents-panel'))}
        <div class="card">
          <div class="srow">
            <div class="tx"><div class="lab">{s('web-settings-agents-panel')}</div><div class="desc">{d('web-settings-agents-panel-description')}</div></div>
            <label class="switch"><input type="checkbox" data-setting="agents-panel" checked={settings['agents-panel']} onchange={(ev) => onFlag('agents-panel', ev)} /><span class="knob"></span></label>
          </div>
        </div>
      {/if}

      {#if shows('about', s('settings-section-about'))}
        <div class="card">
          <div class="srow">
            <div class="tx"><div class="lab">{s('settings-section-about')}</div><div class="desc">{about}</div></div>
          </div>
          <div class="srow">
            <div class="tx"><div class="lab">{s('web-settings-build')}</div><div class="desc">{import.meta.env.MODE}</div></div>
          </div>
        </div>
      {/if}

      {#if panel.error !== ''}<div class="err">{panel.error}</div>{/if}
    </div>
  </div>
{/if}
