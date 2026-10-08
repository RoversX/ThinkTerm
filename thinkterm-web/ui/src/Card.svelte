<script lang="ts">
  // The desktop's surface over a terminal another device holds: opaque,
  // across the whole terminal area, with the desktop's eyes. What it says
  // and what its button offers are the model's (thinkterm-web/src/app.rs
  // `card`); a press anywhere on it, or a scroll, asks for the terminal,
  // and the model's next state says how that went.
  import { handle } from './client';
  import { views } from './client.svelte';
  import { focusTerminal } from './mobile.svelte';

  const card = $derived(views.status.card);
  const taking = $derived(card?.state === 'taking');

  // The desktop's eyes blink once every five seconds.
  let closed = $state(false);
  $effect(() => {
    if (!card) return;
    const timer = setInterval(() => {
      closed = true;
      setTimeout(() => { closed = false; }, 250);
    }, 5000);
    return () => clearInterval(timer);
  });

  function ask(ev: Event) {
    ev.preventDefault();
    if (taking) return;
    handle.client?.take_over();
    // The keyboard stays the terminal's: a button that took focus would
    // keep the keys once the card is gone.
    focusTerminal();
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div id="card" hidden={!card} data-state={card?.state ?? ''} onpointerdown={ask} onwheel={ask}>
  <div class="eyes">{closed ? '─  ─' : '•  •'}</div>
  <div class="title">{card?.title ?? ''}</div>
  <div class="hint">{card?.hint ?? ''}</div>
  <button class="go" disabled={taking} onpointerdown={(ev) => { ev.preventDefault(); ev.stopPropagation(); }} onclick={ask}>{card?.action ?? ''}</button>
</div>
