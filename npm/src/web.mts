// Browser (and bundler) entry point: the wasm has to be loaded with
// `await init()` (or `initSync()`) before the first call.

import initWasm, * as glue from "../web/pdfdiet_wasm.js";
import { type Api, makeApi, type ParamsMatch } from "./core.mjs";

// Fails to compile if the glue's parameters drift from what core expects.
type Assert<T extends [true, true]> = T;
type _Check = Assert<ParamsMatch<typeof glue>>;

export type { CompressOptions, CompressResult, FailureReason, ImageInfo } from "./core.mjs";
export { PdfdietError } from "./core.mjs";

/**
 * Where to load the `.wasm` from: a URL (string or `URL`), a `Request`, a
 * `Response` or a promise of one (e.g. `fetch(url)`), the bytes, or a
 * compiled `WebAssembly.Module`. Typed loosely so the declarations need no
 * DOM types. The form wasm-bindgen documents, `{ module_or_path: ... }`,
 * is accepted too.
 */
export type WasmSource = string | object;

/**
 * What {@link initSync} takes: the bytes or a compiled `WebAssembly.Module`
 * (or wasm-bindgen's `{ module: ... }`).
 */
export type WasmModuleSource = object;

let ready: Promise<void> | undefined;
let loaded = false;

// A plain object carrying `key` is wasm-bindgen's own argument form, which
// 0.1.x documented: pass it through. Anything else is the source itself.
function isPlain(x: unknown, key: string): boolean {
  return (
    typeof x === "object" && x !== null && Object.getPrototypeOf(x) === Object.prototype && key in x
  );
}

/**
 * Load the wasm. Call (and await) it once before {@link compress} or
 * {@link inspect}; later calls return the same promise, and a failed load
 * can be retried.
 *
 * By default the file is fetched from `web/pdfdiet_wasm_bg.wasm` inside
 * the package (bundlers that understand `new URL(..., import.meta.url)`
 * copy it). Pass `wasm` to load it from elsewhere.
 *
 * The promise rejects with a `TypeError` for a plain object that is not
 * `{ module_or_path }` (such as initSync's `{ module }`).
 */
export function init(wasm?: WasmSource): Promise<void> {
  // A plain object must be wasm-bindgen's form; `{}` or initSync's
  // `{ module }` here is a mistake, not something to fetch around.
  if (
    typeof wasm === "object" &&
    wasm !== null &&
    Object.getPrototypeOf(wasm) === Object.prototype &&
    !("module_or_path" in wasm)
  ) {
    return Promise.reject(
      new TypeError("pdfdiet: init() takes a URL, Response, bytes or WebAssembly.Module"),
    );
  }
  // The casts: WasmSource is deliberately looser than the glue's DOM types.
  const arg =
    wasm === undefined
      ? undefined
      : isPlain(wasm, "module_or_path")
        ? (wasm as never)
        : { module_or_path: wasm as never };
  ready ??= initWasm(arg).then(
    () => {
      loaded = true;
    },
    (err: unknown) => {
      ready = undefined;
      throw err;
    },
  );
  return ready;
}

/**
 * Load the wasm synchronously from bytes you already have (or a compiled
 * `WebAssembly.Module`). Browsers may refuse to compile a module this
 * large synchronously on the main thread; use it in workers, or prefer
 * {@link init}.
 *
 * @throws {Error} while an {@link init} call is still loading (await it
 *   instead); once loaded, further calls do nothing.
 */
export function initSync(wasm: WasmModuleSource): void {
  if (loaded) return;
  // Instantiating now would race the pending load, which would replace
  // this instance when it finishes.
  if (ready) throw new Error("pdfdiet: init() is still loading; await it instead");
  glue.initSync(isPlain(wasm, "module") ? (wasm as never) : { module: wasm as never });
  loaded = true;
  ready = Promise.resolve();
}

export default init;

const api = makeApi(glue, () => {
  if (!loaded) throw new Error("pdfdiet: call `await init()` before using it in the browser");
});

/**
 * Shrink a PDF: downsample and re-encode images drawn above the target
 * resolution, and optimize fonts and structure losslessly.
 *
 * @param input The PDF file's bytes (a Node `Buffer` is fine).
 * @throws {PdfdietError} when the document is not processed.
 * @throws {TypeError} for an argument of the wrong type.
 * @throws {RangeError} for an out-of-range `dpi` or `quality`.
 */
export const compress: Api["compress"] = api.compress;
/**
 * List the images in a PDF, with the largest size each is drawn at and the
 * resulting effective resolution.
 *
 * @throws {PdfdietError} when the input cannot be read as a PDF.
 * @throws {TypeError} when `input` is not a `Uint8Array`.
 */
export const inspect: Api["inspect"] = api.inspect;
