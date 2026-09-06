// Runs the wasm-probe stages in Node with stub imports.
// The __wbindgen_* imports come from getrandom's wasm_js backend; the probe
// stages never draw randomness, so inert stubs are sufficient here. A real
// browser build goes through wasm-bindgen CLI instead.
const fs = require('fs');
const path = process.argv[2];
const buf = fs.readFileSync(path);

let memory = null;
const stubs = {
  __wbindgen_placeholder__: {
    __wbg_getRandomValues_38a1ff1ea09f6cc7: () => { throw new Error('getRandomValues stub called'); },
    __wbindgen_object_drop_ref: () => {},
    __wbindgen_describe: () => {},
  },
  __wbindgen_externref_xform__: {
    __wbindgen_externref_table_set_null: () => {},
    __wbindgen_externref_table_grow: (d) => 0,
  },
};

function readPanic(inst) {
  try {
    const ptr = inst.exports.panic_msg_ptr();
    const len = inst.exports.panic_msg_len();
    if (len > 0) {
      const bytes = new Uint8Array(inst.exports.memory.buffer, ptr, len);
      return Buffer.from(bytes).toString('utf8');
    }
  } catch (e) {}
  return '(no panic message)';
}

WebAssembly.instantiate(buf, stubs).then(({ instance }) => {
  instance.exports.install_panic_hook();
  const stages = [
    ['stage1_alloc', 64],
    ['stage2_parser', 13],
    ['stage3_palette', 256],
    ['stage4_terminal', 24],
    ['stage5_advance', 0],
    // "abcdef", cursor to col 3, ECH with u32::MAX: the clamp leaves "ab" (len 2).
    // Unfixed 32-bit release would wrap and erase nothing, leaving len 6.
    ['stage6_ech_overflow', 2],
    // The session layer: a render push applied through the fake host, its
    // dirty rows fetched, predictive echo, and the input-serial rule.
    ['stage7_pane_session', 0],
    // The ordered input drain: folding, wire order, settlement.
    ['stage8_input_queue', 0],
  ];
  let failed = 0;
  for (const [name, expect] of stages) {
    try {
      const got = instance.exports[name]();
      const ok = got === expect;
      if (!ok) failed++;
      console.log(`${ok ? 'PASS' : 'FAIL'} ${name}: got ${got}, expect ${expect}`);
    } catch (e) {
      failed++;
      console.log(`FAIL ${name}: trap ${e.message}; panic: ${readPanic(instance)}`);
    }
  }
  process.exit(failed ? 1 : 0);
}).catch(e => { console.error('instantiate failed:', e.message); process.exit(2); });
