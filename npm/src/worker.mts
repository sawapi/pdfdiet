// Worker entry point (browsers and bundlers): the same functions as the
// browser build, run in a Web Worker so the page stays responsive.

import { connect, type Port, type WorkerApi } from "./protocol.mjs";

export type { CompressOptions, CompressResult, FailureReason, ImageInfo } from "./core.mjs";
export { PdfdietError } from "./core.mjs";
export type { WorkerApi } from "./protocol.mjs";

/** Options for {@link createWorker}. */
export interface WorkerOptions {
  /**
   * Where the worker loads the `.wasm` from: a URL (string or `URL`,
   * relative ones resolved against the page), the bytes (`ArrayBuffer` or
   * `Uint8Array`, copied), or a compiled `WebAssembly.Module`. Unlike
   * `init`, a `Response` or a promise cannot be sent to a worker. By
   * default, the file inside the package.
   */
  wasm?: string | object | undefined;
}

// What can be sent to the worker for init(), with URLs made absolute (a
// relative one would resolve against the worker's script, and a URL
// object cannot be cloned). undefined when `wasm` is none of those.
function wasmSource(wasm: unknown): { value: unknown } | undefined {
  if (wasm === undefined) return { value: undefined };
  if (typeof wasm === "string" || (typeof URL !== "undefined" && wasm instanceof URL)) {
    const base = (globalThis as { location?: { href: string } }).location?.href;
    try {
      return { value: new URL(wasm, base).href };
    } catch {
      return undefined; // not a URL
    }
  }
  if (
    wasm instanceof ArrayBuffer ||
    wasm instanceof Uint8Array ||
    (typeof WebAssembly !== "undefined" && wasm instanceof WebAssembly.Module)
  ) {
    return { value: wasm };
  }
  return undefined;
}

/**
 * Start a Web Worker that runs {@link WorkerApi.compress | compress} and
 * {@link WorkerApi.inspect | inspect}, and return them as functions that
 * take the same arguments and return promises. The worker loads the wasm
 * by itself; there is no `init` to call.
 *
 * Arguments are copied to the worker (your buffers stay usable); the PDF
 * that `compress` returns is moved back without a copy. Calls on one
 * worker run one at a time, in order: create several workers to run in
 * parallel. Errors reject the promise with the same kind of error the
 * function throws, `PdfdietError` with its `reason` included.
 *
 * @throws {TypeError} where Web Workers are not available (such as Node).
 */
export function createWorker(options?: WorkerOptions): WorkerApi {
  if (typeof Worker === "undefined") throw new TypeError("pdfdiet: createWorker needs Web Workers");
  let problem: Error | undefined;
  let source: { value: unknown } | undefined;
  if (options !== undefined && (typeof options !== "object" || options === null || Array.isArray(options))) {
    problem = new TypeError("pdfdiet: options must be an object");
  } else {
    source = wasmSource(options?.wasm);
    if (!source) {
      problem = new TypeError(
        "pdfdiet: wasm must be a URL, an ArrayBuffer, a Uint8Array or a WebAssembly.Module",
      );
    }
  }
  if (problem || !source) {
    // No worker to start: a client whose every call rejects with the reason.
    const none: Port = { postMessage() {}, onmessage: null, onmessageerror: null };
    const c = connect(none, undefined, () => {});
    c.fail(problem ?? new TypeError("pdfdiet: invalid options"));
    return c.api;
  }
  // Written out in full so bundlers see the worker and bundle it.
  const worker = new Worker(new URL("./worker-main.mjs", import.meta.url), { type: "module" });
  const c = connect(worker as unknown as Port, source.value, () => worker.terminate());
  worker.onerror = (e: ErrorEvent) => {
    e.preventDefault();
    c.fail(new Error("pdfdiet: the worker failed" + (e.message ? `: ${e.message}` : " to start")));
  };
  return c.api;
}
