import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import { flushSync } from 'svelte';
import { compileModule } from 'svelte/compiler';
import * as runes from 'svelte/internal/client';
import ts from 'typescript';

// Exercise the actual state module with delayed sockets and clients. No
// DOM or mux server is needed to reproduce a superseded connection.
const path = new URL('../src/machines.svelte.ts', import.meta.url);
const plain = ts.transpileModule(readFileSync(path, 'utf8'), {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
}).outputText;
const compiled = compileModule(plain, { filename: 'machines.svelte.js', generate: 'client' }).js.code;

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function client() {
  return {
    freed: false,
    up: true,
    set_shown() {},
    close() {},
    free() { this.freed = true; },
    connected() { return this.up; },
  };
}

async function setup(start) {
  const clients = new Map();
  const handle = { client: null };
  const changes = [];
  const sockets = [];
  class Socket {
    constructor() { sockets.push(this); }
    close() { this.closed = true; }
    send() {}
  }
  const context = vm.createContext({
    window: { thinkterm: handle },
    localStorage: { getItem: () => null, setItem() {} },
    location: { protocol: 'https:', host: 'example' },
    WebSocket: Socket,
    setTimeout,
  });
  const dependencies = {
    'svelte/internal/client': {
      ...runes,
      // Svelte only proxies objects from its own realm, not the VM's.
      proxy: (value) => runes.proxy(structuredClone(value)),
    },
    svelte: { flushSync },
    './client': { clients, handle },
    './client.svelte': {
      activate(next) { handle.client = next; },
      attach(_client, changed) { changes.push(changed); },
      refreshViews() {},
      s: (key) => key,
    },
    './mobile.svelte': { focusTerminal() {}, openSide() {} },
  };
  const module = new vm.SourceTextModule(compiled, { context });
  await module.link((name) => {
    const exports = dependencies[name];
    assert.ok(exports, `unexpected import ${name}`);
    return new vm.SyntheticModule(Object.keys(exports), function () {
      for (const [key, value] of Object.entries(exports)) this.setExport(key, value);
    }, { context });
  });
  await module.evaluate();
  const api = module.namespace;
  handle.client = client();
  api.configure({ start, prepare() {}, token: '' }, handle.client);
  return { api, clients, handle, changes, sockets };
}

test('switching machines moves the mounted panel subscription to the active client', async (t) => {
  const next = client();
  const { api, clients, handle } = await setup(async () => next);
  await api.connect('server-a', false);
  const calls = [];
  for (const [key, client] of clients) {
    client.set_right_panel_shown = (shown) => {
      assert.equal(client.freed, false);
      calls.push([key, shown]);
    };
  }
  // App's keyed block remounts the panel; its effect captures the handle.
  const destroy = runes.effect_root(() => runes.render_effect(() => {
    void api.machines.active;
    runes.effect(() => {
      const client = handle.client;
      client.set_right_panel_shown(true);
      return () => client.set_right_panel_shown(false);
    });
  }));
  t.after(destroy);
  flushSync();
  assert.deepEqual(calls, [[api.HERE, true]]);

  calls.length = 0;
  api.showMachine('server-a');
  assert.equal(handle.client, next);
  assert.deepEqual(calls, [[api.HERE, false], ['server-a', true]]);

  calls.length = 0;
  api.close('server-a');
  assert.equal(handle.client, clients.get(api.HERE));
  assert.deepEqual(calls, [['server-a', false], [api.HERE, true]]);
  assert.equal(next.freed, true);
});

test('an old start failure cannot fail the replacement connection', async () => {
  const pending = [];
  const { api, clients } = await setup(() => {
    const call = deferred();
    pending.push(call);
    return call.promise;
  });
  const first = api.connect('server-a', false);
  api.close('server-a');
  const second = api.connect('server-a', false);
  pending[0].reject(new Error('old connection closed'));
  await first;
  assert.equal(api.machines.open[0].state, 'connecting');
  const next = client();
  pending[1].resolve(next);
  await second;
  assert.equal(api.machines.open[0].state, 'ready');
  assert.equal(clients.get('server-a'), next);
});

test('close settles an opener even if the socket never emits close', async () => {
  const { api, sockets } = await setup(async (_canvas, _field, open) => {
    await open();
    return client();
  });
  const opening = api.connect('server-a', false);
  const oldMessage = sockets[0].onmessage;
  api.close('server-a');
  await opening;
  assert.equal(sockets[0].closed, true);
  assert.equal(api.machines.open.length, 0);

  const replacement = api.connect('server-a', false);
  oldMessage({ data: JSON.stringify({ op: 'failed', reason: 'declined', message: 'old failure' }) });
  assert.equal(api.machines.open[0].failure, null);
  sockets[1].onmessage({ data: JSON.stringify({ op: 'ready' }) });
  await replacement;
  assert.equal(api.machines.open[0].state, 'ready');
});

test('a retained opener cannot reconnect a closed generation', async () => {
  const pending = [];
  const { api, sockets } = await setup((_canvas, _field, open) => {
    const call = { ...deferred(), open };
    pending.push(call);
    return call.promise;
  });
  const first = api.connect('server-a', false);
  api.close('server-a');
  const second = api.connect('server-a', false);
  await assert.rejects(pending[0].open(), /closed/);
  assert.equal(sockets.length, 0);
  pending[0].reject(new Error('closed'));
  pending[1].resolve(client());
  await Promise.all([first, second]);
  assert.equal(api.machines.open[0].state, 'ready');
});

test('a retired view callback cannot update the new client', async () => {
  const { api, clients, changes } = await setup(async () => client());
  await api.connect('server-a', false);
  api.close('server-a');
  await api.connect('server-a', false);
  clients.get('server-a').up = false;
  changes[0]();
  assert.equal(api.machines.open[0].state, 'ready');
  changes[1]();
  assert.equal(api.machines.open[0].state, 'reconnecting');
});

test('asking to see a reconnecting machine shows it now, and leaves no flag behind', async () => {
  const { api, clients, handle, changes } = await setup(async () => client());
  await api.connect('server-a', false);
  clients.get('server-a').up = false;
  changes[0]();
  assert.equal(api.machines.open[0].state, 'reconnecting');
  await api.connect('server-a', true);
  assert.equal(handle.client, clients.get('server-a'));
  assert.equal(api.machines.open[0].showWhenReady, false);
});
