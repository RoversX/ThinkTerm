// Fails when the wasm-bindgen glue cannot start the bundle's wasm.
// Usage: node ci/check-web-wasm.mjs path/to/thinkterm_web_bg.wasm
//
// The glue grows the table exported as __wbindgen_externrefs and stores JS
// values in it. An old wasm-opt (binaryen 108) re-points that export at the
// fixed-size funcref table, and the page then stays blank in every browser.
import { readFileSync } from 'node:fs';

const path = process.argv[2];
const module = new WebAssembly.Module(readFileSync(path));
const imports = {};
for (const { module: name, name: field } of WebAssembly.Module.imports(module)) {
  (imports[name] ??= {})[field] = () => {
    throw new Error(`unexpected call to ${field} while checking`);
  };
}
const table = new WebAssembly.Instance(module, imports).exports.__wbindgen_externrefs;
if (!(table instanceof WebAssembly.Table)) {
  console.error(`${path}: no __wbindgen_externrefs table export`);
  process.exit(1);
}
try {
  table.set(table.grow(4), true);
} catch (e) {
  console.error(`${path}: __wbindgen_externrefs is not a growable externref table: ${e.message}`);
  process.exit(1);
}
