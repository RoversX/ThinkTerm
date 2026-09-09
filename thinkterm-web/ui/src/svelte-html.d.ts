// `autocorrect` is in svelte/elements for <input> only; #kbd is a <textarea>
// and Safari honours it there too, so tell svelte-check the attribute exists.
declare namespace svelteHTML {
  interface HTMLAttributes<T> {
    autocorrect?: 'on' | 'off';
  }
}
