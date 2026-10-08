<script lang="ts">
  // The page while it starts: the app's mark with a ring turning round it,
  // its name, and the start's steps as a terminal would list them -- a
  // tick and the time each took once done, a spinner on the one under way
  // -- until the client is attached; then it fades away. A start that
  // fails stays, the step it failed on crossed, why under it and a way to
  // try again. Not drawn for the probes (`?check=`), which read their one
  // line from #status.
  import { views } from './client.svelte';
  import { fade, fly } from 'svelte/transition';
  import { cubicInOut, cubicOut } from 'svelte/easing';
  import appIcon from '../../../assets/icon/ThinkTerm_simple_128.png';

  /** Leaving: a fade with the slightest lift towards the viewer. It waits
      a moment first: the client draws its first frame as it attaches, and
      that holds the page for a while -- a fade started at once lost its
      first half to it and read as a jump to half gone. */
  function away(_node: Element, { duration, delay }: { duration: number; delay: number }) {
    return { duration, delay, easing: cubicInOut, css: (t: number) => `opacity: ${t}; transform: scale(${1 + (1 - t) * 0.03})` };
  }

  const probe = new URLSearchParams(location.search).has('check');

  /** The steps, in the order `views.bootStage` counts them (main.ts). */
  const STEPS = ['runtime', 'fonts', 'connect'];
  /** When each step began, to say how long the finished ones took. */
  const began: number[] = [performance.now()];
  let took = $state<string[]>([]);
  $effect(() => {
    const stage = views.ready ? STEPS.length : views.bootStage;
    const now = performance.now();
    for (let i = took.length; i < stage; i++) {
      took[i] = `${((now - (began[i] ?? now)) / 1000).toFixed(1)}s`;
      began[i + 1] = now;
    }
  });
  const stage = $derived(views.ready ? STEPS.length : views.bootStage);

</script>

{#if !views.ready && !probe}
  <div id="boot" class:failed={views.bootFailed} out:away={{ duration: 520, delay: 180 }} role="status" aria-live="polite">
    <div class="dots"></div>
    <div class="dots lit"></div>
    <div class="dots wave"></div>
    <div class="mid">
      <div class="orb">
        {#if !views.bootFailed}<div class="ring" aria-hidden="true"></div>{/if}
        <img class="mark" src={appIcon} alt="" width="64" height="64" draggable="false" />
      </div>
      <div class="name">ThinkTerm</div>
      <div class="log">
        {#each STEPS.slice(0, Math.min(stage + 1, STEPS.length)) as step, i (step)}
          <div class="ln" class:done={i < stage} class:bad={views.bootFailed && i === stage} in:fly={{ y: 6, duration: 280, easing: cubicOut }}>
            <span class="st">
              {#if i < stage}<span class="tick">✓</span>{:else if views.bootFailed}<span class="cross">✗</span>{:else}<span class="spin" aria-hidden="true"></span>{/if}
            </span>
            <span class="what">{step}</span>
            <span class="t">{i < stage ? (took[i] ?? '') : ''}</span>
          </div>
        {/each}
      </div>
      {#if views.bootFailed}
        <div class="why" in:fade={{ duration: 200 }}>{views.boot}</div>
        <button class="again" type="button" onclick={() => location.reload()}>Try again</button>
      {/if}
    </div>
  </div>
{/if}
