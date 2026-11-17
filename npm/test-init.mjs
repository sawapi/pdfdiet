// One way of loading the wasm per process (see the list in test.mjs):
// `node test-init.mjs <entry>:<case>`. Prints "<entry>:<case> ok" on success.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const which = process.argv[2];
const bytes = readFileSync(new URL("./web/pdfdiet_wasm_bg.wasm", import.meta.url));
const response = () => new Response(bytes, { headers: { "content-type": "application/wasm" } });

// Proves the wasm is live: garbage comes back as a PdfdietError from Rust.
function works(lib) {
  assert.throws(
    () => lib.inspect(new Uint8Array([1, 2, 3])),
    (e) => e instanceof lib.PdfdietError && e.reason === "invalid-pdf",
  );
}

const [entry, form] = which.split(":");
const lib = await import(entry === "web" ? "@sawapi/pdfdiet/web" : "@sawapi/pdfdiet");

switch (form) {
  case "before-init":
    assert.throws(() => lib.compress(new Uint8Array(8)), /init\(\)/);
    assert.throws(() => lib.inspect(new Uint8Array(8)), /init\(\)/);
    break;
  case "init-bytes":
    await lib.init(bytes);
    break;
  case "init-module":
    await lib.init(new WebAssembly.Module(bytes));
    break;
  case "init-response":
    await lib.init(response());
    break;
  case "init-promise":
    await lib.init(Promise.resolve(response()));
    break;
  case "init-legacy-object":
    await lib.init({ module_or_path: bytes });
    break;
  case "init-default-export":
    await lib.default(bytes);
    break;
  case "initSync-bytes":
    lib.initSync(bytes);
    break;
  case "initSync-module":
    lib.initSync(new WebAssembly.Module(bytes));
    break;
  case "initSync-legacy-object":
    lib.initSync({ module: bytes });
    break;
  case "init-retry-after-failure":
    await assert.rejects(lib.init(new Uint8Array([0, 1, 2, 3])));
    await lib.init(bytes);
    break;
  case "init-after-initSync":
    lib.initSync(bytes);
    await lib.init(); // already loaded: no fetch (which would fail here)
    break;
  case "initSync-after-init":
    await lib.init(bytes);
    lib.initSync(new Uint8Array([0])); // already loaded: ignored
    break;
  case "initSync-while-loading": {
    const pending = lib.init(bytes);
    assert.throws(() => lib.initSync(bytes), /still loading/);
    await pending;
    break;
  }
  case "init-wrong-object":
    // Our own check, not the TypeError WebAssembly would throw for `{}`.
    const wrong = { name: "TypeError", message: /^pdfdiet: init\(\) takes/ };
    await assert.rejects(lib.init({}), wrong);
    await assert.rejects(lib.init({ module: bytes }), wrong);
    await lib.init(bytes);
    break;
  case "require":
    // The browser build through require() (Node 22.12 or later), then loaded.
    {
      const { createRequire } = await import("node:module");
      const req = createRequire(import.meta.url)("@sawapi/pdfdiet/web");
      assert.equal(req.compress, lib.compress);
      await req.init(bytes);
    }
    break;
  case "browser-style-calls":
    // Code written for the browser build, run under Node: all no-ops.
    await lib.init("https://example.com/pdfdiet_wasm_bg.wasm");
    await lib.default({ module_or_path: bytes });
    lib.initSync(bytes);
    break;
  default:
    throw new Error(`unknown case ${which}`);
}
if (form !== "before-init") works(lib);
console.log(`${which} ok`);
