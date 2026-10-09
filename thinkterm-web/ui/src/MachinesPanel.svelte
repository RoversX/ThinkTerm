<script lang="ts">
  // The Remote Hosts page: the machines this page's server can reach, the
  // ones open here, and -- while one opens -- how far it got and what the
  // relay needs answered (machines.svelte.ts). The desktop's Remote Hosts
  // page, for a page whose only way out is its server, and a tab of its
  // own as that one is: in the terminal's place while it is on show, kept
  // as it was while another tab is.
  import { s, sCount } from './client.svelte';
  import { check, chevronRight, download, house, keyRound, plus, search, server, shieldAlert, unplug, x } from './icons';
  import {
    addMachine, answer, close, closePanel, connect, forgetMachine, HERE, machines, openPlainSsh, showMachine,
    stateText, type Ask, type MachineEntry, type OpenMachine,
  } from './machines.svelte';

  /** What the toolbar's search field holds. */
  let query = $state('');
  /** A machine the search leaves on show: its name or where it is. */
  function found(label: string, endpoint: string): boolean {
    const q = query.trim().toLowerCase();
    return q === '' || label.toLowerCase().includes(q) || endpoint.toLowerCase().includes(q);
  }

  /** The machines open here, and the ones not, in the server's order: the
      hosts saved on the desktop or added here, and the ones from
      `~/.ssh/config`, which the desktop's page groups apart and folds. */
  const opened = $derived(machines.open.filter((m) => found(m.label, endpointOf(m.id))));
  const others = $derived(machines.list.filter((m) => !machines.open.some((o) => o.id === m.id) && found(m.label, m.endpoint)));
  const own = $derived(others.filter((m) => m.source !== 'ssh-config'));
  const system = $derived(others.filter((m) => m.source === 'ssh-config'));
  /** The `~/.ssh/config` group is unfolded: folded by default, as on the
      desktop, and open while a search could find something in it. */
  let systemOpen = $state(false);
  const systemShown = $derived(systemOpen || query.trim() !== '');

  const SOURCE: Record<string, string> = {
    'ssh-config': 'web-machines-from-ssh-config',
    saved: 'web-machines-from-desktop',
    web: 'web-machines-from-web',
  };

  let saving = $state(false);
  let form = $state({ label: '', host: '', port: '', user: '', password: '' });
  /** What is typed into the question on show, per machine. */
  let typed = $state<Record<string, string>>({});
  let remember = $state<Record<string, boolean>>({});
  /** The machines whose details are unfolded. */
  let unfolded = $state<Record<string, boolean>>({});
  let page = $state<HTMLElement>();

  // What is typed belongs to the question it was typed for. One that ends
  // any other way than by its answer -- the socket lost, the machine
  // closed or tried again -- takes it along: a half-typed password is not
  // offered to the next question, which may show its answer in clear.
  let askedLast = new Map<string, Ask | null>();
  $effect(() => {
    const now = new Map<string, Ask | null>();
    for (const m of machines.open) {
      now.set(m.id, m.ask);
      if (askedLast.get(m.id) !== m.ask) typed[m.id] = '';
    }
    askedLast = now;
  });

  // A form put away -- or closed with the tab -- takes what was typed in it.
  $effect(() => {
    if (machines.adding) return;
    form = { label: '', host: '', port: '', user: '', password: '' };
  });

  // A tab closed starts the next one afresh: no search left in its field,
  // and the `~/.ssh/config` group folded again.
  $effect(() => {
    if (machines.tab) return;
    query = '';
    systemOpen = false;
  });

  // Reaching other machines turned off: no host can be added.
  $effect(() => {
    if (!machines.enabled) machines.adding = false;
  });

  // On show, the page takes the keys -- unless a field in it already has
  // them (a question's answer).
  $effect(() => {
    if (!machines.panel || !page) return;
    if (!page.contains(document.activeElement)) page.focus({ preventScroll: true });
  });

  async function submit(ev: Event) {
    ev.preventDefault();
    if (form.host.trim() === '' || saving) return;
    saving = true;
    const ok = await addMachine(form);
    saving = false;
    if (ok) machines.adding = false;
  }

  /** Connect, or try again. The button takes the keys as it is pressed
      (Safari leaves them where they were): the field they came from --
      the blank form Add Remote Host opens on -- is no longer being typed
      in, and the machine comes on show when it is ready. */
  function go(ev: MouseEvent, id: string) {
    (ev.currentTarget as HTMLElement).focus();
    void connect(id, true);
  }

  function reply(m: OpenMachine, value: string | null) {
    answer(m.id, value, remember[m.id] ?? true);
    typed[m.id] = '';
  }

  function onAnswerKey(ev: KeyboardEvent, m: OpenMachine) {
    if (ev.key === 'Enter') {
      ev.preventDefault();
      reply(m, typed[m.id] ?? '');
    }
  }

  // Escape puts the form away, and closes the page when there is none, as
  // on the desktop.
  function onKeydown(ev: KeyboardEvent) {
    // Nothing typed here reaches the terminal's keyboard field.
    ev.stopPropagation();
    if (ev.key !== 'Escape') return;
    ev.preventDefault();
    if (machines.adding) {
      machines.adding = false;
      page?.focus({ preventScroll: true });
    } else {
      closePanel();
    }
  }

  // A field that comes up takes the keys, but not from another field the
  // person is typing in: a question for another machine waits its turn.
  function typeHere(node: HTMLInputElement) {
    const typing = document.activeElement;
    if (typing instanceof HTMLInputElement && typing !== node && typing.closest('#machines')) return;
    node.focus();
  }

  /** The form comes into view whole, its buttons too, when it opens under
      a long list. */
  function reveal(node: HTMLElement) {
    node.scrollIntoView({ block: 'nearest' });
  }

  /** The question's heading. */
  function askTitle(m: OpenMachine): string {
    switch (m.ask?.kind) {
      case 'host-key': return s('web-machines-ask-host-key');
      case 'install': return s('web-machines-ask-install');
      case 'replace': return s('web-machines-ask-replace');
      case 'stop-server': return s('web-machines-ask-stop');
      case 'password': return m.ask.prompt || s('web-machines-ask-password');
      case 'secret': return m.ask.prompt || s('web-machines-ask-code');
      default: return m.ask?.prompt || s('web-machines-ask-answer');
    }
  }

  /** The yes of a yes-or-no question, and its explanation. */
  const CONFIRM: Record<string, [string, string]> = {
    'host-key': ['web-machines-trust', 'web-machines-ask-host-key-hint'],
    install: ['web-machines-install', 'web-machines-ask-install-hint'],
    replace: ['web-machines-replace', 'web-machines-ask-replace-hint'],
    'stop-server': ['web-machines-stop', 'web-machines-ask-stop-hint'],
  };

  function sourceOf(m: MachineEntry): string {
    return s(SOURCE[m.source] ?? 'web-machines-from-web');
  }

  function endpointOf(id: string): string {
    return machines.list.find((m) => m.id === id)?.endpoint ?? '';
  }

  /** Where the form would connect, as the desktop's host form heads itself
      with it: user@host, and the port when it is not 22. */
  const formEndpoint = $derived.by(() => {
    const host = form.host.trim();
    if (host === '') return '';
    const port = form.port.trim();
    const user = form.user.trim();
    return `${user !== '' ? `${user}@` : ''}${host}${port !== '' && port !== '22' ? `:${port}` : ''}`;
  });

  /** An open machine's card takes the grid's full width while it has more
      to say than its row: a question, a failure or its progress log. */
  function wide(m: OpenMachine): boolean {
    return !!m.ask || (m.state === 'failed' && !!m.failure) || (m.log.length > 0 && (m.state !== 'ready' || !!unfolded[m.id]));
  }
</script>

<!-- The desktop's Remote Hosts page (ssh_hosts_view.rs) where it puts it,
     in the terminal's place (`#machines` in tokens.css): New Host and the
     search field in the toolbar, a grid of host cards under their group
     captions, and the host form as an inspector on the right. A card says
     what the machine is and how far it got; its question, failure and log
     open the card to the grid's full width, at its text. Kept while its
     tab is behind another, so a half-typed form or answer waits there. -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
{#if machines.tab}
  <div id="machines" class:adding={machines.adding} hidden={!machines.panel} tabindex="-1" onkeydown={onKeydown} bind:this={page}>
    <div class="lists">
    <div class="tools">
      {#if machines.enabled}<button class="pillbtn primary addbtn" type="button" data-action="add-machine" onclick={() => (machines.adding = true)}>{@html plus}<span>{s('ssh-new-host')}</span></button>{/if}
      <label class="find">{@html search}<input type="search" bind:value={query} placeholder={s('ssh-search')} spellcheck="false" autocapitalize="off" autocomplete="off" /></label>
    </div>
    <div class="body">
      <div class="label">{s('web-machines-connected')}</div>
      <div class="grid">
        <div class="hcard" class:on={machines.active === HERE}>
          <div class="mrow" data-machine={HERE}>
            <span class="logo">{@html house}<span class="dot ok"></span></span>
            <div class="tx"><div class="lab">{s('web-machines-here')}</div>{#if machines.here}<div class="desc">{machines.here}</div>{/if}</div>
            <div class="ctl">
              {#if machines.active !== HERE}<button class="pillbtn" type="button" onclick={() => { showMachine(HERE); closePanel(); }}>{s('web-machines-show')}</button>{:else}<span class="shown">{@html check}</span>{/if}
            </div>
          </div>
        </div>
        {#each opened as m (m.n)}
          {@const state = stateText(m)}
          {@const endpoint = endpointOf(m.id)}
          <div class="group hcard" class:on={machines.active === m.id} class:wide={wide(m)}>
            <div class="mrow" data-machine={m.id} data-state={m.ask ? 'asking' : m.state}>
              <span class="logo">{@html server}<span class="dot" class:ok={m.state === 'ready' && !m.ask} class:busy={(m.state === 'connecting' || m.state === 'reconnecting') && !m.ask} class:wait={!!m.ask} class:bad={m.state === 'failed'}></span></span>
              <div class="tx">
                <div class="lab">{m.label}</div>
                <div class="desc">{#if endpoint}<span>{endpoint}</span>{/if}{#if state !== ''}<span class="st">{state}</span>{#if m.percent !== null}<span class="st">{m.percent}%</span>{/if}{/if}</div>
                {#if m.percent !== null && !m.ask}
                  <div class="bar"><span style={`width: ${m.percent}%`}></span></div>
                {/if}
              </div>
              <div class="ctl">
                {#if m.state === 'ready' || m.state === 'reconnecting'}
                  {#if machines.active !== m.id}<button class="pillbtn" type="button" onclick={() => { showMachine(m.id); closePanel(); }}>{s('web-machines-show')}</button>{:else}<span class="shown">{@html check}</span>{/if}
                {:else if m.state === 'failed'}
                  <button class="pillbtn" type="button" data-action="retry" onclick={(ev) => go(ev, m.id)}>{s('web-machines-retry')}</button>
                {/if}
                <button class="iconbtn" type="button" title={s('web-machines-disconnect')} onclick={() => close(m.id)}>{@html unplug}</button>
              </div>
            </div>
            {#if m.ask}
              {@const confirm = CONFIRM[m.ask.kind]}
              <div class="ask" data-ask={m.ask.kind}>
                <div class="ask-title">{@html confirm ? (m.ask.kind === 'host-key' ? shieldAlert : download) : keyRound}<span>{askTitle(m)}</span></div>
                {#if confirm}
                  <div class="hint">{s(confirm[1])}</div>
                  {#if m.ask.detail}<pre class="detail">{m.ask.detail}</pre>{/if}
                  <div class="btns">
                    <button class="pillbtn" type="button" data-answer="no" onclick={() => reply(m, null)}>{s('web-machines-not-now')}</button>
                    <button class="pillbtn primary" type="button" data-answer="yes" onclick={() => reply(m, 'yes')}>{s(confirm[0])}</button>
                  </div>
                {:else}
                  {#if m.ask.detail}<div class="hint">{m.ask.detail}</div>{/if}
                  <input
                    class="field"
                    type={m.ask.kind === 'text' ? 'text' : 'password'}
                    autocomplete="off"
                    spellcheck="false"
                    value={typed[m.id] ?? ''}
                    oninput={(ev) => (typed[m.id] = (ev.currentTarget as HTMLInputElement).value)}
                    onkeydown={(ev) => onAnswerKey(ev, m)}
                    use:typeHere
                  />
                  <div class="foot">
                    {#if m.ask.remember}
                      <!-- The settings window's switch, before its label. -->
                      <label class="check"><span class="switch"><input type="checkbox" checked={remember[m.id] ?? true} onchange={(ev) => (remember[m.id] = (ev.currentTarget as HTMLInputElement).checked)} /><span class="knob"></span></span><span>{s('web-machines-remember')}</span></label>
                    {/if}
                    <div class="btns">
                      <button class="pillbtn" type="button" data-answer="cancel" onclick={() => reply(m, null)}>{s('web-machines-cancel')}</button>
                      <button class="pillbtn primary" type="button" data-answer="submit" onclick={() => reply(m, typed[m.id] ?? '')}>{s('web-machines-continue')}</button>
                    </div>
                  </div>
                {/if}
              </div>
            {/if}
            {#if m.state === 'failed' && m.failure}
              <div class="fail">
                <!-- A refusal or a cancel says all there is in its own words;
                     anything else carries what the server found. -->
                {#if m.failure.message && m.failure.reason !== 'declined' && m.failure.reason !== 'cancelled'}<div class="hint">{m.failure.message}</div>{/if}
                {#if m.failure.ssh_domain}
                  <div class="btns"><button class="pillbtn" type="button" data-action="plain-ssh" onclick={() => openPlainSsh(m.id)}>{s('web-machines-plain-ssh')}</button></div>
                {/if}
              </div>
            {/if}
            {#if m.log.length > 0 && (m.state !== 'ready' || unfolded[m.id])}
              <button class="more" class:open={unfolded[m.id]} type="button" aria-expanded={!!unfolded[m.id]} onclick={() => (unfolded[m.id] = !unfolded[m.id])}>{@html chevronRight}<span>{s('web-machines-details')}</span></button>
              {#if unfolded[m.id]}<pre class="log">{m.log.join('\n')}</pre>{/if}
            {/if}
          </div>
        {/each}
      </div>

      {#snippet hostCard(m: MachineEntry)}
        <div class="hcard">
          <div class="mrow" data-machine={m.id}>
            <span class="logo">{@html server}</span>
            <div class="tx">
              <div class="lab">{m.label}</div>
              <div class="desc"><span>{m.endpoint}</span><span>{sourceOf(m)}</span>{#if m.password}<span>{s('web-machines-password-kept')}</span>{/if}</div>
            </div>
            <div class="ctl">
              <button class="pillbtn" type="button" data-action="connect" onclick={(ev) => go(ev, m.id)}>{s('web-machines-connect')}</button>
              {#if m.forgettable}
                <button class="iconbtn" type="button" title={s('web-machines-forget')} data-action="forget" onclick={() => void forgetMachine(m.id)}>{@html x}</button>
              {/if}
            </div>
          </div>
        </div>
      {/snippet}
      {#if machines.listed && !machines.enabled}
        <!-- Turned off at the server: what to turn on, and where. -->
        <div class="empty off" data-machines="off">{@html server}<span class="offt">{s('web-machines-off')}</span><span>{s('web-machines-off-hint')}</span></div>
      {:else}
        <!-- The desktop's groups: the hosts saved there or added here, then
             `~/.ssh/config`'s under a caption that folds them, folded at
             first and open while searching. -->
        {#if own.length > 0}
          <div class="label">{s('ssh-group-hosts')}</div>
          <div class="grid">{#each own as m (m.id)}{@render hostCard(m)}{/each}</div>
        {/if}
        {#if system.length > 0}
          <button class="label fold" class:open={systemShown} type="button" data-action="system-hosts" aria-expanded={systemShown} onclick={() => (systemOpen = !systemOpen)}>{@html chevronRight}<span>{sCount('ssh-system-hosts', system.length)}</span></button>
          {#if systemShown}<div class="grid">{#each system as m (m.id)}{@render hostCard(m)}{/each}</div>{/if}
        {/if}
        {#if machines.listed && machines.list.length === 0}
          <!-- The desktop's empty page: the server glyph over one line. -->
          <div class="empty">{@html server}<span>{s('web-machines-empty')}</span></div>
        {/if}
      {/if}
      {#if machines.error !== ''}<div class="err">{machines.error}</div>{/if}
    </div>
    </div>

    {#if machines.adding}
      <!-- The desktop's inspector: the host's name (or what this is) and
           where it would connect, over one card of fields, with the way
           through at its foot. "optional" is said in the field, as a
           placeholder. -->
      <form class="add" onsubmit={submit} use:reveal>
        <div class="ihead">
          <div class="ititle">{form.label.trim() !== '' ? form.label.trim() : s('ssh-new-host')}</div>
          {#if formEndpoint !== ''}<div class="iend">{formEndpoint}</div>{/if}
        </div>
        <div class="card">
          <div class="cap">{s('ssh-group-connection')}</div>
          <div class="pair">
            <label class="fld"><span>{s('web-machines-host')}</span><input bind:value={form.host} spellcheck="false" autocapitalize="off" autocomplete="off" use:typeHere /></label>
            <label class="fld port"><span>{s('web-machines-port')}</span><input bind:value={form.port} inputmode="numeric" placeholder="22" autocomplete="off" /></label>
          </div>
          <label class="fld"><span>{s('web-machines-user')}</span><input bind:value={form.user} spellcheck="false" autocapitalize="off" autocomplete="off" /></label>
          <label class="fld"><span>{s('web-machines-name')}</span><input bind:value={form.label} placeholder={s('web-machines-optional')} spellcheck="false" autocomplete="off" /></label>
          <label class="fld"><span>{s('web-machines-password')}</span><input type="password" bind:value={form.password} placeholder={s('web-machines-optional')} autocomplete="new-password" /></label>
        </div>
        <div class="btns">
          <button class="pillbtn" type="button" onclick={() => (machines.adding = false)}>{s('web-machines-cancel')}</button>
          <button class="pillbtn primary" type="submit" disabled={saving || form.host.trim() === ''}>{s('web-machines-add-connect')}</button>
        </div>
      </form>
    {/if}
  </div>
{/if}
