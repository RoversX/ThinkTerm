// The handle the wasm client hands back once attached. The chrome reads
// its views through it; nothing here is set until `start()` resolves.
import type { Client } from '../../www/pkg/thinkterm_web.js';

export type { Client };

export const handle: { client: Client | null } = { client: null };

/** Every client the page holds, by machine (machines.svelte.ts): this
    server's and one per other machine it has open. */
export const clients = new Map<string, Client>();

/** Each of them, for what applies to all: a preference, the language. */
export function everyClient(): Client[] {
  return clients.size > 0 ? [...clients.values()] : handle.client ? [handle.client] : [];
}

// For probes and the smoke tests, which drive the page from outside.
declare global {
  interface Window {
    thinkterm: { client: Client | null };
  }
}
window.thinkterm = handle;
