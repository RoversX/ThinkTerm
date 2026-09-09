<script lang="ts">
  // A passing remark, bottom right, gone after a few seconds; it stays
  // while the connection is down, which is not a remark but the state.
  // Before the client exists it carries the boot's progress instead, which
  // is where the probes (?check=…) leave their one-line result.
  import { views } from './client.svelte';

  const LIFE = 4000;

  const toast = $derived(views.status.toast);
  // Only moves when a toast's life runs out; reading it is what makes the
  // text below go away on its own.
  let now = $state(performance.now());
  $effect(() => {
    const t = toast;
    if (!t || t.sticky) return;
    const left = t.at + LIFE - performance.now();
    if (left <= 0) {
      now = performance.now();
      return;
    }
    const timer = setTimeout(() => { now = performance.now(); }, left);
    return () => clearTimeout(timer);
  });

  const text = $derived.by(() => {
    if (!views.ready) return views.boot;
    if (!toast) return '';
    return toast.sticky || now < toast.at + LIFE ? toast.text : '';
  });
</script>

<div id="status" hidden={text === ''} class:error={views.bootFailed} data-summary={views.status.summary}>{text}</div>
