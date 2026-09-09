<script lang="ts">
  // The desktop's words over a terminal another device holds.
  import { views } from './client.svelte';

  const card = $derived(views.status.card);
</script>

<div id="card" hidden={!card}><div class="eyes">•  •</div><div class="title">{card?.title ?? ''}</div><div class="hint">{card?.hint ?? ''}</div></div>
