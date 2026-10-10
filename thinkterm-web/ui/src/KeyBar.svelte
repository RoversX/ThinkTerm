<script lang="ts">
  // The key bar a phone types with: the keys a terminal needs and a soft
  // keyboard has not got, along the foot of the visual viewport so the soft
  // keyboard itself never covers it. Every press goes to the terminal
  // through `client.key`, which is a keydown on #kbd by another name
  // (thinkterm-web/src/bridge.rs); Ctrl and Alt are sticky, and apply to
  // the next key from the bar or from the soft keyboard, whichever comes.
  import { handle } from './client';
  import { KEYBAR, toggleKeyboard } from './mobile.svelte';
  import { keyboard } from './icons';

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
  /** How long the visual viewport has to hold still before the canvas is
      cut to it: longer than the gap between the keyboard's animation
      steps, shorter than a person notices. */
  const SETTLE = 150;
  const EVERY = 60;

  /** The sticky modifiers: armed by a tap, spent by the next key. */
  let ctrl = $state(false);
  let alt = $state(false);

  /** Where the bar sits: the foot of the *visual* viewport, which is what
      the soft keyboard takes a bite out of. */
  let top = $state(0);
  /** The bar itself, so what it measures -- KEYBAR plus whatever the home
      indicator's safe area adds under it -- is what the page leaves room for. */
  let bar = $state<HTMLDivElement | null>(null);

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

  // iOS does not stop at pointerdown: once the finger lifts it plays the
  // tap as mouse events and a click, and focuses what was tapped -- the bar
  // itself, which has a tabindex -- so #kbd lost focus and the keyboard the
  // button had just brought up went straight back down. Cancelling the
  // touch's end cancels all of that; the press was handled already.
  function onTouchEnd(ev: TouchEvent) {
    if (ev.target instanceof Element && ev.target.closest('.k')) ev.preventDefault();
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
    let settle: ReturnType<typeof setTimeout> | null = null;
    let applied = 0;
    /** The bar's height when the canvas was last cut to fit above it. */
    let appliedMine = 0;
    /** The height the canvas had before the soft keyboard cut it down;
        zero while it has it. */
    let rest = 0;
    /** Grown back to `rest` as the keyboard began to go: the canvas stays
        where it is and the keyboard slides off its last rows, as a phone's
        own apps do. Riding down with the bar instead showed blank above it
        until the cut, a beat after the keyboard had gone. */
    let ahead = false;
    let lastHeight = 0;
    const root = document.documentElement;
    /** Until the canvas is cut to the viewport's new height, it rides with
        the bar (--term-shift in tokens.css): its last row -- the prompt --
        stays on the bar's edge while the keyboard comes or goes, and the
        rows the cut adds or takes are at the top, where the server will
        put them. Left where it was, the prompt fell behind the keyboard as
        it came up, and the terminal jumped down once it went. */
    const ride = (barTop: number) => {
      const shift = applied > 0 && !ahead ? barTop - (applied - appliedMine) : 0;
      root.style.setProperty('--term-shift', `${Math.round(shift)}px`);
    };
    /** The height the canvas is cut to, once the viewport has stopped
        moving. The soft keyboard slides in over a few hundred milliseconds
        and the viewport reports every step of it; cutting the canvas at
        each step resized the terminal that many times, and every resize
        reflowed and redrew it -- the flicker seen when the keyboard came
        up. The bar itself still follows every step, so it never floats
        away from the keyboard's edge. */
    const apply = (height: number, mine: number) => {
      root.style.setProperty('--keybar', `${mine}px`);
      appliedMine = mine;
      // Cut to fit, the canvas sits where the bar is: nothing to ride.
      root.style.setProperty('--term-shift', '0px');
      if (height === applied) return;
      if (height < applied && rest === 0) rest = applied;
      if (height >= rest) rest = 0;
      applied = height;
      root.style.setProperty('--vvh', `${height}px`);
    };
    const read = () => {
      // The page never pans: a browser that moves the view up to show the
      // focused field (which sits at the terminal's cursor) took the tab
      // rows off the top and left a gap over the bar. Where the browser
      // resizes the page for the keyboard instead (interactive-widget in
      // index.html) this never fires.
      if (vv && (vv.offsetTop > 0 || window.scrollY > 0)) window.scrollTo(0, 0);
      const height = vv ? vv.height : window.innerHeight;
      const mine = bar?.offsetHeight || KEYBAR;
      // Kept in a local: read back from `top`, the state would make this
      // effect depend on it, and run again -- cutting the canvas at once
      // -- every time the bar moved.
      const barTop = (vv ? vv.offsetTop : 0) + height - mine;
      top = barTop;
      // The keyboard going: back to the whole height at once, one cut.
      if (height < lastHeight) ahead = false;
      if (!ahead && rest > 0 && height > applied) {
        ahead = true;
        apply(rest, mine);
      }
      lastHeight = height;
      ride(barTop);
      if (settle !== null) clearTimeout(settle);
      settle = setTimeout(() => {
        settle = null;
        ahead = false;
        apply(height, mine);
      }, SETTLE);
    };
    root.style.setProperty('--keybar', `${KEYBAR}px`);
    // The first cut is immediate: nothing is moving yet, and the terminal
    // must not sit at the window's height for SETTLE before it fits.
    apply(vv ? vv.height : window.innerHeight, bar?.offsetHeight || KEYBAR);
    read();
    // The first read runs before the bar has been laid out with its safe
    // area, so its real height is taken on the next frame.
    requestAnimationFrame(read);
    vv?.addEventListener('resize', read);
    vv?.addEventListener('scroll', read);
    window.addEventListener('resize', read);
    return () => {
      if (settle !== null) clearTimeout(settle);
      vv?.removeEventListener('resize', read);
      vv?.removeEventListener('scroll', read);
      window.removeEventListener('resize', read);
      root.style.removeProperty('--keybar');
      root.style.removeProperty('--vvh');
      root.style.removeProperty('--term-shift');
    };
  });

  // The ground around the bar is the terminal's own (tokens.css,
  // --term-bg), so the capsule floats on the terminal rather than on a
  // strip of the page's colour. It is the colour the client on show clears
  // its canvas to (`data-bg`); showing another machine moves `#term` to
  // that machine's canvas.
  $effect(() => {
    const root = document.documentElement;
    const read = () => {
      const bg = document.getElementById('term')?.dataset.bg ?? '';
      if (/^#[0-9a-fA-F]{6}$/.test(bg)) root.style.setProperty('--term-bg', bg);
    };
    const watch = new MutationObserver(read);
    watch.observe(document.body, { subtree: true, attributes: true, attributeFilter: ['data-bg', 'id'] });
    read();
    return () => {
      watch.disconnect();
      root.style.removeProperty('--term-bg');
    };
  });

  $effect(() => stopRepeat);
</script>

<!-- The keys are the bar's, delegated, as the tab row's and the sidebar's
     are; a release anywhere ends a repeat. -->
<svelte:window onpointerup={stopRepeat} onpointercancel={stopRepeat} />
<div bind:this={bar} id="keybar" role="toolbar" aria-label="Terminal keys" tabindex="-1" style="top:{top.toFixed(2)}px" onpointerdown={onDown} ontouchend={onTouchEnd}>
  <!-- The soft keyboard's own button stays put at the start; the keys
       scroll beside it. -->
  <div class="pill">
  <button class="k kbd" type="button" data-key="kbd" aria-label="Keyboard">{@html keyboard}</button>
  <div class="keys">
  {#each HEAD as k (k.name)}
    <button class="k" type="button" data-key={k.name} aria-label={k.label}>{k.label}</button>
  {/each}
  <button class="k" class:armed={ctrl} type="button" data-key="Control" aria-pressed={ctrl}>Ctrl</button>
  <button class="k" class:armed={alt} type="button" data-key="Alt" aria-pressed={alt}>Alt</button>
  {#each REST as k (k.name)}
    <button class="k" type="button" data-key={k.name} aria-label={k.name}>{k.label}</button>
  {/each}
  </div>
  </div>
</div>
