<script lang="ts">
  // The settings panel: the page's own preferences, one row each. The wasm
  // holds them (thinkterm-web/src/settings.rs) and applies the language and
  // the font itself; the theme is painted here, and the rest is read by
  // whatever it concerns.
  import { handle } from './client';
  import { s, views } from './client.svelte';
  import { info, minus, palette, panelLeft, plus, rotateCcw, search, slidersHorizontal, x } from './icons';
  import { applyTheme, closeSettings, FOLLOW_DESKTOP, loadSchemes, panel, pickScheme, previewScheme, schemes, setSetting, storedScheme } from './settings.svelte';
  import type { Hotkey, Scheme, ScrollMode, Theme } from './model';
  import { POP, WINDOW, ms } from './motion';
  import { fade, scale } from 'svelte/transition';
  import { cubicOut } from 'svelte/easing';

  /** Fade and an 8px rise: the settings window arriving, and leaving the
      way it came. */
  function rise(_node: Element, { duration }: { duration: number }) {
    return {
      duration,
      easing: cubicOut,
      css: (t: number) =>
        `opacity: ${t}; transform: var(--settings-tf) translateY(${(1 - t) * 8}px)`,
    };
  }

  const settings = $derived(views.settings);

  // The window's sections, as the desktop's settings window lists them
  // on the left; the page has fewer rows, so fewer sections.
  // The panels and the plugins are both under Sidebar: whatever a plugin
  // shows, it shows in a panel.
  type Section = 'general' | 'appearance' | 'sidebar' | 'about';
  const SECTIONS: [Section, string, string][] = [
    ['general', 'settings-section-general', slidersHorizontal],
    ['appearance', 'settings-section-appearance', palette],
    ['sidebar', 'settings-section-sidebar', panelLeft],
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
  const SCROLL_MODES: [ScrollMode, string][] = [
    ['smooth', 'web-scroll-mode-smooth'],
    ['stepped', 'web-scroll-mode-stepped'],
  ];
  // The shortcuts as they are pressed, not as they are stored.
  const HOTKEYS: [Hotkey, string][] = [
    ['cmd-k', '⌘K'],
    ['cmd-shift-p', '⌘⇧P'],
    ['ctrl-shift-p', 'Ctrl+Shift+P'],
  ];

  // The theme is the page's to paint, whether the panel is open or not.
  $effect(() => applyTheme(settings.theme));

  // The plugins the plugin host on the server's machine runs, under the
  // panels: asked for while Sidebar is on show, or a search could find one
  // there. Told only when that changes: a want let go and taken again would
  // close the server's way to the host in between.
  const plugins = $derived(views.plugins);
  let pluginsWanted = false;
  $effect(() => {
    const wanted = panel.open && (section === 'sidebar' || query.trim() !== '');
    if (wanted === pluginsWanted) return;
    pluginsWanted = wanted;
    handle.client?.plugins_want('settings', wanted);
  });
  /** The plugins a search finds, or every one on the section's own page. */
  const pluginRows = $derived(
    query.trim() === '' || shows('sidebar', s('settings-section-plugins'))
      ? plugins.rows
      : plugins.rows.filter((row) => shows('sidebar', row.name)),
  );
  /** Snippets, a built-in plugin: its switch is its panel's. */
  const snippetsRow = $derived(plugins.rows.find((row) => row.builtin && row.id === 'snippets'));
  let reloading = $state(false);

  function onPlugin(id: string, ev: Event) {
    const field = ev.currentTarget;
    if (field instanceof HTMLInputElement) void handle.client?.plugin_set_enabled(id, field.checked);
  }

  /** How long a plugin runs unused, as the desktop's settings offer it. */
  const BACKGROUNDS = [
    ['always', 'settings-plugins-background-always'],
    ['briefly', 'settings-plugins-background-briefly'],
    ['never', 'settings-plugins-background-never'],
  ] as const;

  async function reloadPlugins() {
    reloading = true;
    try {
      await handle.client?.plugins_reload();
    } finally {
      reloading = false;
    }
  }

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

  // The colour-scheme picker: a searchable list over the panel, with the
  // terminal drawn in whatever row is under the cursor. Escape restores
  // what was picked before it opened.
  let picking = $state(false);
  let schemeQuery = $state('');
  let cursor = $state(0);
  const chosen = $derived(settings['terminal-scheme'] ?? FOLLOW_DESKTOP);
  const following = $derived(chosen === FOLLOW_DESKTOP);
  const schemeLabel = $derived(following ? s('web-scheme-desktop') : chosen);

  /** The list: "Follow the desktop" first, then whatever matches. */
  const matches = $derived.by(() => {
    const q = schemeQuery.trim().toLowerCase();
    const rows: (Scheme | null)[] = q === '' || s('web-scheme-desktop').toLowerCase().includes(q) ? [null] : [];
    for (const scheme of schemes.all) {
      if (q === '' || scheme.name.toLowerCase().includes(q)) rows.push(scheme);
    }
    return rows;
  });

  function openPicker() {
    schemeQuery = '';
    cursor = 0;
    picking = true;
    void loadSchemes();
  }

  /** Put back what was picked before the picker opened: the stored
      colours when the catalogue has not arrived yet. */
  function restoreScheme() {
    picking = false;
    previewScheme(following ? null : (schemes.all.find((scheme) => scheme.name === chosen) ?? storedScheme()));
  }

  function commit(row: Scheme | null) {
    picking = false;
    pickScheme(row);
  }

  $effect(() => {
    if (!picking) return;
    // Keep the cursor on the list, and show what it is on.
    const row = matches[Math.min(cursor, matches.length - 1)] ?? null;
    previewScheme(row);
  });

  function onPickerKeydown(ev: KeyboardEvent) {
    ev.stopPropagation();
    if (ev.key === 'Escape') {
      ev.preventDefault();
      restoreScheme();
    } else if (ev.key === 'ArrowDown' || ev.key === 'ArrowUp') {
      ev.preventDefault();
      const next = cursor + (ev.key === 'ArrowDown' ? 1 : -1);
      cursor = Math.max(0, Math.min(matches.length - 1, next));
    } else if (ev.key === 'Enter') {
      ev.preventDefault();
      // No row under the cursor is no choice; `null` is a row of its own.
      if (cursor < matches.length) commit(matches[cursor]);
    }
  }

  function focusField(node: HTMLInputElement) {
    node.focus({ preventScroll: true });
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
  <div id="settings-back" onpointerdown={closeSettings} transition:fade={{ duration: ms(WINDOW) }}></div>
  <!-- The desktop's window comes up rather than in: a fade and an 8px
       rise, at the length a window takes rather than a menu's. -->
  <div
    id="settings"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onkeydown={onKeydown}
    use:focusHere
    transition:rise={{ duration: ms(WINDOW) }}
  >
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

      {#if shows('general', s('settings-language')) || shows('general', s('web-settings-palette-hotkey')) || shows('general', s('web-settings-scroll-mode'))}
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
          {#if shows('general', s('web-settings-scroll-mode'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('web-settings-scroll-mode')}</div><div class="desc">{d('web-settings-scroll-mode-description')}</div></div>
              <select class="pillsel" data-setting="scroll-mode" value={settings['scroll-mode']} onchange={(ev) => setSetting('scroll-mode', (ev.currentTarget as HTMLSelectElement).value)}>
                {#each SCROLL_MODES as [value, label] (value)}
                  <option value={value}>{s(label)}</option>
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

      {#if shows('appearance', s('web-settings-theme')) || shows('appearance', s('web-settings-font')) || shows('appearance', s('web-settings-scheme'))}
        <div class="card">
          {#if shows('appearance', s('web-settings-scheme'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('web-settings-scheme')}</div><div class="desc">{d('web-settings-scheme-description')}</div></div>
              <button class="pillsel" type="button" data-setting="terminal-scheme" data-scheme={chosen} onclick={openPicker}>{schemeLabel}</button>
            </div>
          {/if}
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

      <!-- The right panel's panels, a switch each. Snippets is a built-in
           plugin, and its switch is the plugin's. -->
      {#if shows('sidebar', s('web-settings-agents-panel')) || shows('sidebar', s('right-mode-snippets'))}
        {#if query.trim() === ''}<div class="sdesc">{d('settings-sidebar-description')}</div>{/if}
        <div class="card">
          {#if shows('sidebar', s('web-settings-agents-panel'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('web-settings-agents-panel')}</div><div class="desc">{d('web-settings-agents-panel-description')}</div></div>
              <label class="switch"><input type="checkbox" data-setting="agents-panel" checked={settings['agents-panel']} onchange={(ev) => onFlag('agents-panel', ev)} /><span class="knob"></span></label>
            </div>
          {/if}
          {#if shows('sidebar', s('right-mode-snippets'))}
            <div class="srow">
              <div class="tx"><div class="lab">{s('right-mode-snippets')}</div><div class="desc">{d('settings-sidebar-snippets-description')}</div></div>
              <label class="switch"><input type="checkbox" data-panel-switch="snippets" checked={snippetsRow?.enabled ?? false} disabled={!snippetsRow?.switchable} onchange={(ev) => onPlugin('snippets', ev)} /><span class="knob"></span></label>
            </div>
          {/if}
        </div>
      {/if}

      {#if shows('sidebar', s('settings-section-plugins')) || (query.trim() !== '' && pluginRows.length > 0)}
        {#if query.trim() === ''}<div class="sdesc">{d('settings-plugins-description')}</div>{/if}
        <div class="card">
          <div class="srow">
            <div class="tx"><div class="lab">{s('settings-plugins-reload')}</div><div class="desc">{d('settings-plugins-reload-description')}</div></div>
            <button class="pillbtn" type="button" data-action="reload-plugins" disabled={reloading} onclick={reloadPlugins}>{s('settings-plugins-reload-button')}</button>
          </div>
        </div>
        {#if plugins.state === 'ready' && plugins.status !== ''}<div class="err">{plugins.status}</div>{/if}
        <div class="card" data-plugins={plugins.state}>
          {#each pluginRows as row (row.key)}
            <div class="srow" data-plugin={row.id}>
              <div class="tx"><div class="lab">{row.name}</div><div class="desc">{row.detail}</div></div>
              {#if row.builtin}
                <!-- Its switch is its panel's, above. -->
              {:else if row.new}
                <!-- Never let run: a press made for it lets it, from where it is. -->
                <button class="pillbtn" type="button" data-plugin-allow={row.id} onclick={() => void handle.client?.plugin_set_enabled(row.id, true)}>{s('settings-plugins-allow')}</button>
              {:else if row.switchable}
                <div class="ctl">
                  {#if row.background}
                    <!-- How long it runs unused, beside its switch. -->
                    <select class="pillsel" data-plugin-background={row.id} title={row.background_detail} value={row.background} onchange={(ev) => void handle.client?.plugin_set_background(row.id, (ev.currentTarget as HTMLSelectElement).value)}>
                      {#each BACKGROUNDS as [value, label] (value)}<option {value}>{s(label)}</option>{/each}
                    </select>
                  {/if}
                  <label class="switch"><input type="checkbox" data-plugin-switch={row.id} checked={row.enabled} onchange={(ev) => onPlugin(row.id, ev)} /><span class="knob"></span></label>
                </div>
              {:else}
                <span class="pilltag">{s('settings-plugins-unusable')}</span>
              {/if}
            </div>
          {:else}
            <div class="srow"><div class="tx"><div class="desc">{plugins.status}</div></div></div>
          {/each}
        </div>
        {#if plugins.refused}<div class="err">{plugins.refused}</div>{/if}
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
  {#if picking}
    <div id="schemes-back" onpointerdown={restoreScheme} transition:fade={{ duration: ms(POP) }}></div>
    <div
      id="schemes"
      role="dialog"
      aria-modal="true"
      tabindex="-1"
      onkeydown={onPickerKeydown}
      transition:scale={{ duration: ms(POP), start: 0.97, opacity: 0 }}
    >
      <input class="q" placeholder={s('web-scheme-search')} bind:value={schemeQuery} spellcheck="false" oninput={() => (cursor = 0)} use:focusField />
      <div class="pl">
        {#each matches as row, i (row ? row.name : '')}
          <div class="pe" class:on={i === Math.min(cursor, matches.length - 1)} data-scheme={row ? row.name : FOLLOW_DESKTOP}
               onpointerenter={() => (cursor = i)} onclick={() => commit(row)}>
            <span class="sw" style:background={row ? row.background : 'transparent'}>
              {#if row}{#each row.ansi as colour, n (n)}<i style:background={colour}></i>{/each}{/if}
            </span>
            <span class="t">{row ? row.name : s('web-scheme-desktop')}</span>
            {#if (row ? row.name : FOLLOW_DESKTOP) === chosen}<span class="acc">{s('web-scheme-preview')}</span>{/if}
          </div>
        {:else}
          <div class="pn">{schemes.loading ? '…' : schemes.error || s('command-palette-empty')}</div>
        {/each}
      </div>
    </div>
  {/if}
{/if}
