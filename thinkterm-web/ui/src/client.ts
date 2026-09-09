// The handle the wasm client hands back once attached. The chrome reads
// its views through it; nothing here is set until `start()` resolves.
import type { Client } from '../../www/pkg/thinkterm_web.js';

export type { Client };

export const handle: { client: Client | null } = { client: null };

// For probes and the smoke tests, which drive the page from outside.
declare global {
  interface Window {
    thinkterm: { client: Client | null };
  }
}
window.thinkterm = handle;
