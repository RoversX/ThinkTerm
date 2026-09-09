<script lang="ts">
  // The key bar a phone types with: the keys a terminal needs and a soft
  // keyboard has not got, along the foot of the visual viewport so the soft
  // keyboard itself never covers it. Every press goes to the terminal
  // through `client.key`, which is a keydown on #kbd by another name
  // (thinkterm-web/src/bridge.rs); Ctrl and Alt are sticky, and apply to
  // the next key from the bar or from the soft keyboard, whichever comes.
  import { handle } from './client';
  import { KEYBAR } from './mobile.svelte';

  /** A key: `name` is the DOM name the wasm maps, `label` what it says. */
  type Key = { name: string; label: string };

  /** Ahead of the modifiers. */
  const HEAD: Key[] = [
    { name: 'Escape', label: 'Esc' },
    { name: 'Tab', label: 'Tab' },
  ];
  /** After them: the arrows, the jumps, and the four characters a soft
      keyboard buries behind a page of symbols. */
  const REST: Key[] = [
    { name: 'ArrowLeft', label: '←' },
    { name: 'ArrowUp', label: '↑' },
    { name: 'ArrowDown', label: '↓' },
    { name: 'ArrowRight', label: '→' },
    { name: 'Home', label: 'Home' },
    { name: 'End', label: 'End' },
    { name: 'PageUp', label: 'PgUp' },
    { name: 'PageDown', label: 'PgDn' },
    { name: '-', label: '-' },
    { name: '/', label: '/' },
    { name: '|', label: '|' },
    { name: '~', label: '~' },
  ];
  /** The keys a long press repeats. */
  const REPEATS = new Set(['ArrowLeft', 'ArrowUp', 'ArrowDown', 'ArrowRight']);
  /** How long an arrow is held before it repeats, and how often it then does. */
  const HOLD = 500;
  const EVERY = 60;

  /** The sticky modifiers: armed by a tap, spent by the next key. */
  let ctrl = $state(false);
  let alt = $state(false);

  /** Where the bar sits: the foot of the *visual* viewport, which is what
      the soft keyboard takes a bite out of. */
  let top = $state(0);

  function disarm() {
    ctrl = false;
    alt = false;
  }

  function send(name: string) {
    handle.client?.key(name, ctrl, alt, false);
    disarm();
  }

  let delay: ReturnType<typeof setTimeout> | null = null;
  let ticker: ReturnType<typeof setInterval> | null = null;

  function stopRepeat() {
    if (delay !== null) {
      clearTimeout(delay);
      delay = null;
    }
    if (ticker !== null) {
      clearInterval(ticker);
      ticker = null;
    }
  }

  function startRepeat(name: string) {
    stopRepeat();
    delay = setTimeout(() => {
      delay = null;
      // The modifiers were spent on the first press; a repeat is the bare key.
      ticker = setInterval(() => handle.client?.key(name, false, false, false), EVERY);
    }, HOLD);
  }

  /** Ask for the soft keyboard, or send it away. */
  function toggleKeyboard() {
    const kbd = document.getElementById('kbd');
    if (!(kbd instanceof HTMLTextAreaElement)) return;
    if (document.activeElement === kbd) kbd.blur();
    else kbd.focus();
  }

  // One delegated handler, as the tab row and the sidebar have. The press
  // must not move focus: #kbd keeps it, so the soft keyboard stays up while
  // the bar is used.
  function onDown(ev: PointerEvent) {
    const hit = ev.target instanceof Element ? ev.target.closest('.k') : null;
    if (!hit) return;
    ev.preventDefault();
    const name = hit.getAttribute('data-key');
    if (name === null) return;
    if (name === 'Control') {
      ctrl = !ctrl;
      return;
    }
    if (name === 'Alt') {
      alt = !alt;
      return;
    }
    if (name === 'kbd') {
      toggleKeyboard();
      return;
    }
    send(name);
    if (REPEATS.has(name)) startRepeat(name);
  }

  // An armed modifier applies to the soft keyboard's next key too. #kbd's
  // own listener is the wasm's and sits on the same element without
  // capture, so this one runs first and takes the key instead.
  $effect(() => {
    const kbd = document.getElementById('kbd');
    if (!(kbd instanceof HTMLTextAreaElement)) return;
    /** The character an armed key was sent for has still to be swallowed. */
    let spent = false;
    const onKeydown = (ev: KeyboardEvent) => {
      if (!ctrl && !alt) return;
      if (ev.key === 'Control' || ev.key === 'Alt' || ev.key === 'Shift' || ev.key === 'Meta') return;
      ev.preventDefault();
      ev.stopImmediatePropagation();
      handle.client?.key(ev.key, ctrl, alt, ev.shiftKey);
      disarm();
      // A soft keyboard inserts its character whatever preventDefault says,
      // and the wasm sends whatever lands in the field as bytes; the insert
      // this key is about to make is not typed a second time.
      spent = true;
      kbd.value = '';
      setTimeout(() => {
        spent = false;
      }, 0);
    };
    const onInput = (ev: Event) => {
      if (!spent) return;
      spent = false;
      ev.stopImmediatePropagation();
      kbd.value = '';
    };
    kbd.addEventListener('keydown', onKeydown, true);
    kbd.addEventListener('input', onInput, true);
    return () => {
      kbd.removeEventListener('keydown', onKeydown, true);
      kbd.removeEventListener('input', onInput, true);
    };
  });

  // The bar follows the visual viewport, and the canvas and the drawers are
  // sized from the same height (--vvh and --keybar in tokens.css): the soft
  // keyboard shrinks that viewport and nothing else.
  $effect(() => {
    const vv = window.visualViewport;
    const read = () => {
      const height = vv ? vv.height : window.innerHeight;
      top = (vv ? vv.offsetTop : 0) + height - KEYBAR;
      document.documentElement.style.setProperty('--vvh', `${height}px`);
    };
    document.documentElement.style.setProperty('--keybar', `${KEYBAR}px`);
    read();
    vv?.addEventListener('resize', read);
    vv?.addEventListener('scroll', read);
    window.addEventListener('resize', read);
    return () => {
      vv?.removeEventListener('resize', read);
      vv?.removeEventListener('scroll', read);
      window.removeEventListener('resize', read);
      document.documentElement.style.removeProperty('--keybar');
      document.documentElement.style.removeProperty('--vvh');
    };
  });

  $effect(() => stopRepeat);
</script>

<!-- The keys are the bar's, delegated, as the tab row's and the sidebar's
     are; a release anywhere ends a repeat. -->
<svelte:window onpointerup={stopRepeat} onpointercancel={stopRepeat} />
<div id="keybar" role="toolbar" aria-label="Terminal keys" tabindex="-1" style="top:{top.toFixed(2)}px" onpointerdown={onDown}>
  {#each HEAD as k (k.name)}
    <button class="k" type="button" data-key={k.name} aria-label={k.label}>{k.label}</button>
  {/each}
  <button class="k" class:armed={ctrl} type="button" data-key="Control" aria-pressed={ctrl}>Ctrl</button>
  <button class="k" class:armed={alt} type="button" data-key="Alt" aria-pressed={alt}>Alt</button>
  {#each REST as k (k.name)}
    <button class="k" type="button" data-key={k.name} aria-label={k.name}>{k.label}</button>
  {/each}
  <button class="k" type="button" data-key="kbd" aria-label="Keyboard">&#9000;</button>
</div>
