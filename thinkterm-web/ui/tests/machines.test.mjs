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

async function setup(start, storage = new Map(), locks) {
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
    navigator: { locks },
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, value),
    },
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

async function replyList(socket, ids, enabled = true) {
  socket.onmessage({ data: JSON.stringify({
    op: 'machines', here: 'gateway', enabled,
    machines: ids.map((id) => ({ id, label: id })),
  }) });
  await new Promise(setImmediate);
}

const openStore = 'thinkterm.machines';

test('a disabled relay keeps the previous visit’s open hosts in order', async () => {
  const ids = ['server-b', 'server-a'];
  const storage = new Map([[openStore, JSON.stringify(ids)]]);
  const off = await setup(async () => client(), storage);
  await replyList(off.sockets[0], [], false);
  assert.equal(off.api.machines.open.length, 0);
  assert.deepEqual(JSON.parse(storage.get(openStore)), ids);

  const on = await setup(async () => client(), storage);
  await replyList(on.sockets[0], ids);
  assert.deepEqual(Array.from(on.api.machines.open, (m) => m.id), ids);
});

test('relay shutdown during a restore keeps the host, but a permanent refusal removes it', async () => {
  for (const [reason, expected] of [['off', ['server-a']], ['declined', []]]) {
    const storage = new Map([[openStore, '["server-a"]']]);
    const tab = await setup(async (_canvas, _field, open) => { await open(); return client(); }, storage);
    await replyList(tab.sockets[0], ['server-a']);
    tab.sockets[1].onmessage({ data: JSON.stringify({ op: 'failed', reason, message: 'Stopped' }) });
    await new Promise(setImmediate);
    assert.equal(tab.api.machines.open[0].state, 'failed');
    assert.deepEqual(JSON.parse(storage.get(openStore)), expected);
  }
});

test('a closing tab wins over a delayed host list and a delayed background connection', async () => {
  const storage = new Map([[openStore, '["server-a"]']]);
  const pending = deferred();
  const closing = await setup(async () => client(), storage);
  const opening = await setup(() => pending.promise, storage);
  const listing = await setup(async () => client(), storage);
  await replyList(closing.sockets[0], ['server-a']);
  await replyList(opening.sockets[0], ['server-a']);
  assert.equal(opening.api.machines.open[0].state, 'connecting');

  closing.api.close('server-a');
  pending.resolve(client());
  await replyList(listing.sockets[0], ['server-a']);
  assert.equal(listing.api.machines.open.length, 0);
  assert.equal(opening.api.machines.open[0].state, 'ready');
  assert.deepEqual(JSON.parse(storage.get(openStore)), []);

  // This tab still has A open. Its next manual connection adds only B.
  await opening.api.connect('server-b', false);
  const next = await setup(async () => client(), storage);
  await replyList(next.sockets[0], ['server-a', 'server-b']);
  assert.deepEqual(Array.from(next.api.machines.open, (m) => m.id), ['server-b']);
});

test('successful manual connections are restored in opening order', async () => {
  const storage = new Map();
  const pending = deferred();
  let starts = 0;
  const { api } = await setup(() => ++starts === 1 ? pending.promise : Promise.resolve(client()), storage);
  const first = api.connect('server-a', false);
  await api.connect('server-b', false);
  assert.deepEqual(JSON.parse(storage.get(openStore)), ['server-b']);
  pending.resolve(client());
  await first;
  assert.deepEqual(JSON.parse(storage.get(openStore)), ['server-a', 'server-b']);
});

test('concurrent edits in different tabs share the open-list lock', async () => {
  let turn = Promise.resolve();
  const locks = { request(name, callback) {
    assert.equal(name, openStore);
    turn = turn.then(callback);
    return turn;
  } };
  const storage = new Map([[openStore, '["server-a"]']]);
  const a = await setup(async () => client(), storage, locks);
  const b = await setup(async () => client(), storage, locks);
  await new Promise(setImmediate);
  await replyList(a.sockets[0], ['server-a']);
  await replyList(b.sockets[0], ['server-a']);

  const write = storage.set.bind(storage);
  storage.set = (key, value) => {
    storage.set = write;
    a.api.close('server-a');
    return write(key, value);
  };
  await b.api.connect('server-b', false);
  await turn;
  assert.deepEqual(JSON.parse(storage.get(openStore)), ['server-b']);
});

test('manual connections still work with restricted browser storage', async () => {
  const denied = { request: async () => { throw new Error('Locks blocked'); } };
  const storage = new Map();
  const tab = await setup(async () => client(), storage, denied);
  await tab.api.connect('server-a', false);
  assert.deepEqual(JSON.parse(storage.get(openStore)), ['server-a']);

  const blocked = { get() { throw new Error('Storage blocked'); } };
  const other = await setup(async () => client(), blocked);
  await other.api.connect('server-b', false);
  assert.equal(other.api.machines.open[0].state, 'ready');
  other.api.close('server-b');
  assert.equal(other.api.machines.open.length, 0);
});

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
