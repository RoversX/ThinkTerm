<script lang="ts">
  // A plugin's panel in the right panel, or -- `extended` -- its extended
  // view beside it (PluginExtended): what its player gives to paint
  // (thinkterm-plugin-panel `Painting`, through thinkterm-web/src/
  // plugin_panel.rs), painted on a canvas with the page's own fonts and
  // colours, as the desktop paints it with its own. The pointer and the
  // wheel go back to the wasm, which says whether to paint again; what the
  // plugin draws anew arrives as a new revision. Each of its fields is an
  // input of the page's own, which the browser edits, over the box the
  // canvas paints in order: see-through, and cut away where what answers a
  // press is drawn over it. What it holds and where the keyboard goes go
  // back to the wasm, whose player
  // decides what the plugin hears. The panel has the keyboard only through
  // them, and one that loses it without the user leaves it on the canvas,
  // never the terminal (`hold`).
  import { handle } from './client';
  import { refreshViews, s, views } from './client.svelte';
  import { canExtend } from './extended';
  import { iconByName, x as closeIcon } from './icons';
  import { focusTerminal } from './mobile.svelte';
  import type { PanelOp, PanelView, Painting } from './model';
  import { onMac } from './palette.svelte';

  let { extended = false }: { extended?: boolean } = $props();

  // An extended view's close button: ThinkTerm's, at its top left, where
  // the desktop has the file preview's, whatever the plugin draws. The
  // plugin is told where it is, in the canvas's units, to keep clear of it.
  const CLOSE = { x: 8, y: 4, size: 28 };

  // The page's type scale for a panel, in CSS pixels: the sizes the right
  // panel's own rows use, and a line a little over each.
  const TEXT = {
    small: { size: 12, line: 16 },
    body: { size: 14, line: 20 },
    title: { size: 16, line: 22 },
  } as const;
  const MONO = { size: 13, line: 18 };
  // The terminal's face, which the page fetched for its glyphs; registered
  // for the canvas under a name of its own, with the system's to fall back
  // on until it is.
  const MONO_FACE = 'ThinkTerm Panel Mono';
  const MONO_FAMILY = `"${MONO_FACE}", ui-monospace, Menlo, monospace`;
  const ELLIPSIS = '...';
  // A field of lines' corners (thinkterm-plugin-panel `FIELD_RADIUS`); one
  // on one line is round at its ends, as the panel's search is.
  const FIELD_RADIUS = 6;
  // How long after an input method put its text in a Return marked 229 is
  // still its own, in milliseconds: Safari sends the one that ended it then.
  const ENDED_COMPOSING = 500;

  let host: HTMLDivElement;
  let canvas: HTMLCanvasElement;
  let view = $state<PanelView | null>(null);
  let width = 0;
  let height = 0;
  /** The revision on show, so what is on show already is not painted
      again: a hover repaints here, and its revision comes round after. */
  let painted = -1;

  type Rgba = [number, number, number, number];
  let palette: Record<string, Rgba> | null = null;
  let dark = document.documentElement.dataset.theme !== 'light';

  // ThinkTerm's colours by the names a panel uses (thinkterm-plugin-panel
  // `Token`), from the page's own tokens. The grounds are laid on the
  // panel's ground here, as the desktop lays them: a see-through green
  // reads stronger than the same green mixed.
  const SOURCES: Record<string, string> = {
    text: '--text',
    'text-muted': '--text2',
    'text-faint': '--muted',
    bg: '--side-bg',
    'bg-raised': '--btn',
    'bg-hover': '--row-hover',
    'bg-selected': '--row-sel',
    border: '--sep',
    accent: '--accent',
    positive: '--plus',
    negative: '--minus',
    warning: '--caution',
    thumb: '--muted',
  };
  // The page's own, for a field's box.
  const FIELD_SOURCES: Record<string, string> = {
    field: '--btn',
    'field-border': '--btn-border',
  };

  function parse(css: string): Rgba {
    const m = css.match(/rgba?\(([^)]+)\)/);
    if (!m) return [0, 0, 0, 1];
    const parts = m[1].split(/[\s,/]+/).filter((p) => p !== '').map(Number);
    return [parts[0] ?? 0, parts[1] ?? 0, parts[2] ?? 0, parts[3] ?? 1];
  }

  function resolvePalette(): Record<string, Rgba> {
    const probe = document.createElement('span');
    probe.style.display = 'none';
    host.appendChild(probe);
    const out: Record<string, Rgba> = {};
    for (const [name, variable] of Object.entries({ ...SOURCES, ...FIELD_SOURCES })) {
      probe.style.color = `var(${variable})`;
      out[name] = parse(getComputedStyle(probe).color);
    }
    probe.remove();
    out['on-accent'] = [255, 255, 255, 1];
    const ground = out.bg;
    const [faint, strong] = dark ? [0.16, 0.34] : [0.12, 0.26];
    const mix = (over: Rgba, amount: number): Rgba => [
      Math.round(ground[0] + (over[0] - ground[0]) * amount),
      Math.round(ground[1] + (over[1] - ground[1]) * amount),
      Math.round(ground[2] + (over[2] - ground[2]) * amount),
      1,
    ];
    out['positive-bg'] = mix(out.positive, faint);
    out['negative-bg'] = mix(out.negative, faint);
    out['positive-bg-strong'] = mix(out.positive, strong);
    out['negative-bg-strong'] = mix(out.negative, strong);
    out.thumb = [out.thumb[0], out.thumb[1], out.thumb[2], 0.55];
    return out;
  }

  /** A panel's colour: one of ThinkTerm's by name, or `#rrggbb[aa]`. A
      name the page does not know draws as text, as on the desktop. */
  function rgba(color: string): Rgba {
    if (color.startsWith('#')) {
      const hex = color.slice(1);
      if (/^[0-9a-f]{6}([0-9a-f]{2})?$/i.test(hex)) {
        const byte = (at: number) => parseInt(hex.slice(at, at + 2), 16);
        return [byte(0), byte(2), byte(4), hex.length === 8 ? byte(6) / 255 : 1];
      }
    }
    const pal = palette ?? (palette = resolvePalette());
    return pal[color] ?? pal.text;
  }

  function css([r, g, b, a]: Rgba, alpha = 1): string {
    return `rgba(${r}, ${g}, ${b}, ${a * alpha})`;
  }

  /** The interface font's family, read once a painting: reading a style
      makes the page work its styles out. */
  function uiFamily(): string {
    return getComputedStyle(document.documentElement).getPropertyValue('--ui').trim() || 'system-ui';
  }

  function font(op: Extract<PanelOp, { op: 'text' }>, family: string): string {
    if (op.font === 'mono') return `400 ${MONO.size}px ${MONO_FAMILY}`;
    return `${op.bold ? 600 : 400} ${TEXT[op.size].size}px ${family}`;
  }

  // Text as shown and how wide, remembered by font, text and -- when it is
  // ellipsized -- room: measuring is the costly part of painting a list,
  // and the same rows come back scroll after scroll. Let go of whole once
  // it grows past its bound.
  type Cut = { shown: string; wide: number };
  const cuts = new Map<string, Cut>();
  const CUTS = 4000;

  function cut(ctx: CanvasRenderingContext2D, text: string, room: number, ellipsize: boolean): Cut {
    const key = `${ctx.font}\u0000${ellipsize ? room : -1}\u0000${text}`;
    const known = cuts.get(key);
    if (known !== undefined) return known;
    let shown = text;
    let wide = ctx.measureText(text).width;
    if (ellipsize && wide > room) {
      const chars = Array.from(text);
      const budget = room - ctx.measureText(ELLIPSIS).width;
      let low = 0;
      let high = chars.length;
      while (low < high) {
        const mid = Math.ceil((low + high) / 2);
        if (ctx.measureText(chars.slice(0, mid).join('')).width <= budget) low = mid;
        else high = mid - 1;
      }
      shown = budget > 0 ? chars.slice(0, low).join('') + ELLIPSIS : '';
      wide = ctx.measureText(shown).width;
    }
    if (cuts.size >= CUTS) cuts.clear();
    const result = { shown, wide };
    cuts.set(key, result);
    return result;
  }

  function paint(painting: Painting | null) {
    const ctx = canvas?.getContext('2d');
    if (!ctx) return;
    const scale = devicePixelRatio || 1;
    ctx.setTransform(scale, 0, 0, scale, 0, 0);
    ctx.clearRect(0, 0, width, height);
    canvas.style.cursor = painting?.cursor === 'pointer' ? 'pointer' : 'default';
    if (!painting) return;
    const family = uiFamily();
    let region = -1;
    for (const op of painting.ops) {
      if (op.clip !== region) {
        if (region !== -1) ctx.restore();
        ctx.save();
        const [l, t, r, b] = painting.clips[op.clip] ?? [0, 0, width, height];
        ctx.beginPath();
        ctx.rect(l, t, r - l, b - t);
        ctx.clip();
        region = op.clip;
      }
      paintOp(ctx, op, family);
    }
    if (region !== -1) ctx.restore();
  }

  function paintOp(ctx: CanvasRenderingContext2D, op: PanelOp, family: string) {
    switch (op.op) {
      case 'rect': {
        if (op.w <= 0 || op.h <= 0) return;
        const radius = Math.max(0, Math.min(op.radius, op.w / 2, op.h / 2));
        if (op.fill !== undefined) {
          ctx.fillStyle = css(rgba(op.fill));
          ctx.beginPath();
          ctx.roundRect(op.x, op.y, op.w, op.h, radius);
          ctx.fill();
        }
        if (op.border !== undefined) {
          ctx.strokeStyle = css(rgba(op.border));
          ctx.lineWidth = 1;
          ctx.beginPath();
          ctx.roundRect(op.x + 0.5, op.y + 0.5, op.w - 1, op.h - 1, Math.max(0, radius - 0.5));
          ctx.stroke();
        }
        return;
      }
      case 'text': {
        if (op.text === '' || op.w < 1) return;
        ctx.font = font(op, family);
        ctx.fillStyle = css(rgba(op.color));
        ctx.textBaseline = 'middle';
        const { shown, wide } = cut(ctx, op.text, op.w, op.font === 'ui');
        const left = Math.max(
          op.x,
          op.align === 'center' ? op.x + (op.w - wide) / 2 : op.align === 'right' ? op.x + op.w - wide : op.x,
        );
        const middle = op.y + op.h / 2;
        // The monospaced font is cut at its box's edge, not ellipsized: a
        // plugin lines its columns up itself.
        if (op.font === 'mono' && wide > op.w) {
          ctx.save();
          ctx.beginPath();
          ctx.rect(op.x, op.y, op.w, op.h);
          ctx.clip();
          ctx.fillText(shown, left, middle);
          ctx.restore();
        } else {
          ctx.fillText(shown, left, middle);
        }
        return;
      }
      case 'line': {
        if (op.points.length < 4) return;
        ctx.strokeStyle = css(rgba(op.color));
        ctx.lineWidth = op.width;
        ctx.lineJoin = 'round';
        ctx.lineCap = 'round';
        ctx.beginPath();
        ctx.moveTo(op.points[0], op.points[1]);
        for (let i = 2; i + 1 < op.points.length; i += 2) ctx.lineTo(op.points[i], op.points[i + 1]);
        ctx.stroke();
        return;
      }
      case 'area': {
        if (op.points.length < 4) return;
        const color = rgba(op.color);
        let top = op.base;
        ctx.beginPath();
        ctx.moveTo(op.points[0], op.base);
        for (let i = 0; i + 1 < op.points.length; i += 2) {
          ctx.lineTo(op.points[i], op.points[i + 1]);
          top = Math.min(top, op.points[i + 1]);
        }
        ctx.lineTo(op.points[op.points.length - 2], op.base);
        ctx.closePath();
        if (op.fade && op.base > top) {
          const fade = ctx.createLinearGradient(0, top, 0, op.base);
          fade.addColorStop(0, css(color));
          fade.addColorStop(1, css(color, 0));
          ctx.fillStyle = fade;
        } else {
          ctx.fillStyle = css(color);
        }
        ctx.fill();
        return;
      }
      case 'thumb': {
        ctx.fillStyle = css(rgba('thumb'));
        ctx.beginPath();
        ctx.roundRect(op.x, op.y, op.w, op.h, Math.min(op.w, op.h) / 2);
        ctx.fill();
        return;
      }
      // The box, in order with what is drawn over it; the page's own
      // input over it, see-through, holds the text.
      case 'field': {
        if (op.w <= 0 || op.h <= 0) return;
        const radius = Math.min(fieldRadius(op), op.w / 2);
        ctx.fillStyle = css(rgba('field'));
        ctx.beginPath();
        ctx.roundRect(op.x, op.y, op.w, op.h, radius);
        ctx.fill();
        ctx.strokeStyle = css(rgba(op.focused ? 'accent' : 'field-border'));
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.roundRect(op.x + 0.5, op.y + 0.5, op.w - 1, op.h - 1, Math.max(0, radius - 0.5));
        ctx.stroke();
        return;
      }
    }
  }

  type FieldOp = Extract<PanelOp, { op: 'field' }>;

  function fieldRadius(op: FieldOp): number {
    return Math.max(0, op.kind === 'lines' ? Math.min(FIELD_RADIUS, op.h / 2) : op.h / 2);
  }
  /** The panel's fields, each with an input of the page's own over it. */
  const fields = $derived(
    (view?.painting?.ops ?? []).filter((op): op is FieldOp => op.op === 'field'),
  );

  /** Whether a field of `kind` leaves `ev`'s key to the panel: Escape and
      Tab, the function keys, and on one line the keys that move up and
      down. A key it uses is the field's; one with Ctrl, Alt or Command is
      the field's, the page's or the browser's, never the panel's. */
  function leaves(kind: FieldOp['kind'], ev: KeyboardEvent): boolean {
    if (ev.ctrlKey || ev.altKey || ev.metaKey) return false;
    if (ev.key === 'Escape' || ev.key === 'Tab' || /^F\d{1,2}$/.test(ev.key)) return true;
    return kind !== 'lines' && ['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown'].includes(ev.key);
  }

  /** The keyboard left a field without the user -- the plugin let go of
      it, or the field went -- while they may be typing in it: what they
      type next was meant for the panel, and goes nowhere. It waits on the
      canvas, where Escape gives it to the terminal; a press anywhere puts
      it where it was pressed. */
  function hold() {
    if (canvas?.isConnected) canvas.focus({ preventScroll: true });
    else if (document.activeElement instanceof HTMLElement) document.activeElement.blur();
  }

  /** What of its box a field's input takes: all of it but where what
      answers a press -- a button at its end -- is drawn over it, which
      shows there, and is pressed there. */
  function uncovered(op: FieldOp): string {
    if (!op.covered?.length) return '';
    // The box one way round and the pieces, which do not overlap, the
    // other: they are left out.
    let path = `M0 0H${op.w}V${op.h}H0Z`;
    for (const [x, y, w, h] of op.covered) path += `M${x - op.x} ${y - op.y}v${h}h${w}v${-h}Z`;
    return `path('${path}')`;
  }

  function onCanvasKey(ev: KeyboardEvent) {
    if (ev.key !== 'Escape' || ev.ctrlKey || ev.altKey || ev.metaKey) return;
    ev.preventDefault();
    focusTerminal();
  }

  /** Keeps an input in step with the panel's field it is over: in its
      place, holding what the plugin last put there, and with the keyboard
      when the player says the field has it -- moved there by the plugin,
      or by Tab -- and not when it says it has not. */
  function field(node: HTMLInputElement | HTMLTextAreaElement, op: FieldOp) {
    let taken = -1;
    let current = op;
    // Its button that empties it, where it has one: shown while it holds
    // something, and never taking the keyboard from it.
    const clear = document.createElement('button');
    clear.type = 'button';
    clear.tabIndex = -1;
    clear.className = 'pclear';
    clear.innerHTML = closeIcon;
    node.after(clear);
    const showClear = () => {
      clear.hidden = !current.clear || node.value === '' || node.disabled;
    };
    // What the panel's field takes of what the page's holds: less, it
    // holds that instead.
    const tell = () => {
      const held = handle.client?.plugin_panel_field_text(extended, current.id, node.value);
      if (typeof held === 'string' && held !== node.value) node.value = held;
      showClear();
    };
    const follow = (op: FieldOp) => {
      current = op;
      node.style.left = `${op.x}px`;
      node.style.top = `${op.y}px`;
      node.style.width = `${op.w}px`;
      node.style.height = `${op.h}px`;
      const [ix, iy, iw, ih] = op.inside;
      node.style.padding = `${iy - op.y}px ${op.x + op.w - ix - iw}px ${op.y + op.h - iy - ih}px ${ix - op.x}px`;
      node.style.borderRadius = `${fieldRadius(op)}px`;
      node.style.clipPath = uncovered(op);
      // The browser counts in UTF-16 and the panel in characters, which
      // may be two of those: the panel's field cuts what is over.
      node.maxLength = op.limit * 2;
      node.placeholder = op.placeholder;
      node.dataset.font = op.font;
      if (op.revision !== taken) {
        taken = op.revision;
        if (node.value !== op.text) node.value = op.text;
      }
      if (op.clear) {
        clear.style.left = `${op.clear.x}px`;
        clear.style.top = `${op.clear.y}px`;
        clear.style.width = `${op.clear.size}px`;
        clear.style.height = `${op.clear.size}px`;
      }
      const live = view?.live ?? false;
      if (op.focused && live && document.activeElement !== node) {
        node.focus({ preventScroll: true });
      } else if (
        !op.focused &&
        document.activeElement === node &&
        // To another of its fields, which takes it as it follows.
        !handle.client?.plugin_panel_has_keyboard(extended)
      ) {
        hold();
      }
      // What a plugin starting again last drew takes no typing: nobody
      // would hear it.
      node.disabled = !live;
      showClear();
    };
    follow(op);
    // The canvas outlines the box with the keyboard.
    const onFocus = () => {
      handle.client?.plugin_panel_field_focus(extended, current.id);
      queueMicrotask(() => refresh());
    };
    const onBlur = (ev: FocusEvent) => {
      // To another of the panel's fields: the keyboard stays in it. Nor
      // does it go with the window: the browser gives this field it back.
      const to = ev.relatedTarget;
      if (to instanceof HTMLElement && to.classList.contains('pfield') && host.contains(to)) return;
      if (!document.hasFocus()) return;
      handle.client?.plugin_panel_blur(extended);
      queueMicrotask(() => refresh());
    };
    // What a field holds is told once composed: never the half of a word
    // an input method is still putting together.
    const onInput = (ev: Event) => {
      if ((ev as InputEvent).isComposing) return;
      tell();
    };
    // When an input method last put its text in.
    let composed = -Infinity;
    const onComposed = (ev: CompositionEvent) => {
      composed = ev.timeStamp;
      tell();
    };
    const onClearDown = (ev: MouseEvent) => ev.preventDefault();
    const onClear = () => {
      node.value = '';
      if (document.activeElement !== node) node.focus({ preventScroll: true });
      tell();
    };
    const onKey = (ev: KeyboardEvent) => {
      // Keys pressed while composing are the input method's: marked so, a
      // printable one marked 229 (Chrome on macOS), or the one that ended
      // a composition just now (Safari). An input method on but idle -- an
      // Android keyboard -- marks every key 229, and Return still submits.
      if (ev.isComposing) return;
      if (ev.keyCode === 229) {
        if (Array.from(ev.key).length === 1) return;
        if (ev.timeStamp - composed < ENDED_COMPOSING) {
          composed = -Infinity;
          return;
        }
      }
      // The terminal's keys are not this field's to hear.
      ev.stopPropagation();
      const command = onMac() ? ev.metaKey && !ev.ctrlKey : ev.ctrlKey && !ev.metaKey;
      if (ev.key === 'Enter' && !ev.altKey && (current.kind === 'lines' ? command : !ev.ctrlKey && !ev.metaKey)) {
        ev.preventDefault();
        handle.client?.plugin_panel_field_submit(extended, current.id);
        return;
      }
      if (!leaves(current.kind, ev)) return;
      const taken = handle.client?.plugin_panel_key(extended, ev.key, ev.shiftKey, ev.ctrlKey, ev.altKey, ev.metaKey);
      if (!taken) return;
      ev.preventDefault();
      // The keyboard may have moved to another field, or gone back.
      refresh();
      if (!handle.client?.plugin_panel_has_keyboard(extended) && document.activeElement === node) {
        node.blur();
        focusTerminal();
      }
    };
    // Typed as one element, for its events' types to be known.
    const el: HTMLElement = node;
    el.addEventListener('focus', onFocus);
    el.addEventListener('blur', onBlur);
    el.addEventListener('input', onInput);
    el.addEventListener('compositionend', onComposed);
    el.addEventListener('keydown', onKey);
    clear.addEventListener('mousedown', onClearDown);
    clear.addEventListener('click', onClear);
    return {
      update: follow,
      destroy() {
        if (document.activeElement === node) hold();
        el.removeEventListener('focus', onFocus);
        el.removeEventListener('blur', onBlur);
        el.removeEventListener('input', onInput);
        el.removeEventListener('compositionend', onComposed);
        el.removeEventListener('keydown', onKey);
        clear.removeEventListener('mousedown', onClearDown);
        clear.removeEventListener('click', onClear);
        clear.remove();
      },
    };
  }

  /** Reads the panel as the wasm has it, and paints it: when it changed
      since it was last painted, or `anew`, when the canvas was cleared or
      what it is painted with changed. */
  function refresh(anew = false) {
    const revision = handle.client?.plugin_panel_revision(extended) ?? -1;
    if (!anew && revision === painted) return;
    painted = revision;
    const json = handle.client?.plugin_panel(extended);
    const next = json ? (JSON.parse(json) as PanelView | null) : null;
    view = next;
    paint(next?.painting ?? null);
  }

  /** Tells the wasm how much room there is and how text measures in it,
      and, for the panel, whether there is room beside it for its extended
      view. */
  function sendEnv() {
    const ctx = canvas?.getContext('2d');
    if (!ctx || width <= 0 || height <= 0) return;
    ctx.font = `400 ${MONO.size}px ${MONO_FAMILY}`;
    const advance = ctx.measureText('0'.repeat(20)).width / 20;
    const env = {
      width,
      height,
      scale: devicePixelRatio || 1,
      dark,
      small: TEXT.small,
      body: TEXT.body,
      title: TEXT.title,
      mono: { ...MONO, advance },
      can_extend: !extended && canExtend(),
      close: extended ? CLOSE : undefined,
    };
    const refused = handle.client?.plugin_panel_env(extended, JSON.stringify(env));
    if (refused) console.warn('plugin panel size: ' + refused);
  }

  function fit() {
    const box = host.getBoundingClientRect();
    width = Math.max(0, Math.floor(box.width));
    height = Math.max(0, Math.floor(box.height));
    const scale = devicePixelRatio || 1;
    canvas.width = Math.round(width * scale);
    canvas.height = Math.round(height * scale);
    canvas.style.width = `${width}px`;
    canvas.style.height = `${height}px`;
    sendEnv();
    refresh(true);
  }

  $effect(() => {
    const observer = new ResizeObserver(() => fit());
    observer.observe(host);
    // The theme comes and goes with a mark on the root.
    const theme = new MutationObserver(() => {
      dark = document.documentElement.dataset.theme !== 'light';
      palette = null;
      sendEnv();
      refresh(true);
    });
    theme.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
    // The terminal's face, once: measured again when it is in.
    if (![...document.fonts].some((face) => face.family === MONO_FACE)) {
      const face = new FontFace(MONO_FACE, 'url(./fonts/JetBrainsMono-Regular.ttf)');
      document.fonts.add(face);
      face.load().then(() => {
        cuts.clear();
        sendEnv();
        refresh(true);
      }, () => {});
    }
    // The wheel scrolls the panel and nothing else: a listener that can
    // say so, which Svelte's own is not.
    const onWheel = (ev: WheelEvent) => {
      ev.preventDefault();
      const line = ev.deltaMode === 1 ? 16 : 1;
      let dx = ev.deltaX * (ev.deltaMode === 2 ? width : line);
      let dy = ev.deltaY * (ev.deltaMode === 2 ? height : line);
      // A mouse's wheel turns one way: with Shift, it goes across.
      if (ev.shiftKey && dx === 0) [dx, dy] = [dy, 0];
      const { x, y } = local(ev);
      if (handle.client?.plugin_panel_wheel(extended, x, y, dx, dy)) refresh();
    };
    canvas.addEventListener('wheel', onWheel, { passive: false });
    // The room beside the panel changes with the terminal's width: with
    // the window's, and with either panel's.
    const room = new ResizeObserver(() => sendEnv());
    const term = document.getElementById('term');
    if (!extended && term) room.observe(term);
    fit();
    return () => {
      stopFling();
      observer.disconnect();
      room.disconnect();
      theme.disconnect();
      canvas.removeEventListener('wheel', onWheel);
    };
  });

  // What the plugin drew anew.
  $effect(() => {
    void (extended ? views.extendedRevision : views.panelRevision);
    refresh();
  });

  function local(ev: MouseEvent): { x: number; y: number } {
    const box = canvas.getBoundingClientRect();
    return { x: ev.clientX - box.left, y: ev.clientY - box.top };
  }

  // A finger drags the panel's lists and scroll areas as a wheel turns
  // them, and a flick carries on a little after it lifts, as the terminal's
  // scrollback does (touch.ts). A tap is left alone: the browser sends it
  // as a click. The canvas takes no touch of the browser's own
  // (`touch-action`), so a drag never scrolls the page instead.
  const SLOP = 8;
  const FLING_DECAY = 0.94;
  const FLING_MIN = 0.04;
  type Drag = { id: number; x0: number; y0: number; x: number; y: number; at: number; vx: number; vy: number; moving: boolean };
  let drag: Drag | null = null;
  let fling: number | null = null;

  function stopFling() {
    if (fling === null) return;
    cancelAnimationFrame(fling);
    fling = null;
  }

  /** Scrolls what is under `at` (client pixels) by `dx`, `dy` of its units. */
  function scrollAt(at: { x: number; y: number }, dx: number, dy: number) {
    const box = canvas.getBoundingClientRect();
    if (handle.client?.plugin_panel_wheel(extended, at.x - box.left, at.y - box.top, dx, dy)) refresh();
  }

  function onPointerDown(ev: PointerEvent) {
    if (ev.pointerType !== 'touch') return;
    // A finger on the glass stops what a flick was still scrolling.
    stopFling();
    if (drag) return;
    drag = { id: ev.pointerId, x0: ev.clientX, y0: ev.clientY, x: ev.clientX, y: ev.clientY, at: ev.timeStamp, vx: 0, vy: 0, moving: false };
  }

  function onPointerMove(ev: PointerEvent) {
    if (ev.pointerType === 'touch') {
      if (!drag || ev.pointerId !== drag.id) return;
      if (!drag.moving && Math.hypot(ev.clientX - drag.x0, ev.clientY - drag.y0) < SLOP) return;
      drag.moving = true;
      // Up the glass is down the list, as a wheel turned toward you is.
      const dx = drag.x - ev.clientX;
      const dy = drag.y - ev.clientY;
      const dt = Math.max(1, ev.timeStamp - drag.at);
      drag.vx = 0.8 * (dx / dt) + 0.2 * drag.vx;
      drag.vy = 0.8 * (dy / dt) + 0.2 * drag.vy;
      drag.x = ev.clientX;
      drag.y = ev.clientY;
      drag.at = ev.timeStamp;
      scrollAt(drag, dx, dy);
      return;
    }
    const { x, y } = local(ev);
    if (handle.client?.plugin_panel_pointer(extended, x, y)) refresh();
  }

  function onPointerUp(ev: PointerEvent) {
    if (!drag || ev.pointerId !== drag.id) return;
    const { moving, at, x, y } = drag;
    let { vx, vy } = drag;
    drag = null;
    // A flick carries on; a finger that stopped before lifting does not.
    if (!moving || ev.type === 'pointercancel' || ev.timeStamp - at > 80) return;
    if (Math.hypot(vx, vy) < FLING_MIN) return;
    let last = performance.now();
    const tick = (now: number) => {
      const dt = Math.min(now - last, 64);
      last = now;
      scrollAt({ x, y }, vx * dt, vy * dt);
      const keep = Math.pow(FLING_DECAY, dt / 16);
      vx *= keep;
      vy *= keep;
      fling = Math.hypot(vx, vy) < FLING_MIN ? null : requestAnimationFrame(tick);
    };
    fling = requestAnimationFrame(tick);
  }

  function onPointerLeave(ev: PointerEvent) {
    if (ev.pointerType === 'touch') return;
    if (handle.client?.plugin_panel_leave(extended)) refresh();
  }

  /** The close button: the view goes at once, and its plugin hears so. */
  function onClose(ev: MouseEvent) {
    ev.preventDefault();
    handle.client?.plugin_extended_close();
    refreshViews();
  }

  function onMouseDown(ev: MouseEvent) {
    const { x, y } = local(ev);
    // A press on the panel leaves the keyboard where it is -- in one of its
    // fields, or the terminal -- but for keys held on the canvas, which the
    // terminal has again: a press is the user's.
    ev.preventDefault();
    if (document.activeElement === canvas) focusTerminal();
    const heard = handle.client?.plugin_panel_click(
      extended, x, y, ev.button, Math.max(1, ev.detail), ev.shiftKey, ev.ctrlKey, ev.altKey, ev.metaKey,
    );
    if (heard) refresh();
  }
</script>

<div class="plugin" class:extended bind:this={host}>
  <!-- Focused only to hold the keyboard from the terminal (`hold`). -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <canvas
    bind:this={canvas}
    tabindex="-1"
    onkeydown={onCanvasKey}
    onpointerdown={onPointerDown}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
    onpointerleave={onPointerLeave}
    onmousedown={onMouseDown}
  ></canvas>
  {#each fields as op (op.id)}
    {#if op.kind === 'lines'}
      <textarea class="pfield" spellcheck="false" use:field={op}></textarea>
    {:else}
      <input class="pfield" type={op.kind === 'secret' ? 'password' : 'text'} autocomplete="off" spellcheck="false" use:field={op} />
    {/if}
    {#if op.icon}
      <span class="picon" style="left: {op.icon.x}px; top: {op.icon.y}px; width: {op.icon.size}px; height: {op.icon.size}px">{@html iconByName(op.icon.name) ?? ''}</span>
    {/if}
  {/each}
  {#if view?.message}
    <div class="msg">{view.message}</div>
  {/if}
  {#if extended}
    <button
      class="close"
      type="button"
      title={s('web-settings-close')}
      style="left: {CLOSE.x}px; top: {CLOSE.y}px; width: {CLOSE.size}px; height: {CLOSE.size}px"
      onclick={onClose}
    >{@html closeIcon}</button>
  {/if}
</div>
