// The message protocol between createWorker() (worker.mts) and the worker
// it starts (worker-main.mts). Kept apart from both so it can be tested in
// Node over a MessageChannel, which Web Workers are not available for.
//
// client -> worker: { type: "init", wasm }, then { type: "call", id, fn, args }
// worker -> client: { type: "ready" } or { type: "failed", error },
//                   then { type: "result", id, value } or { type: "error", id, error }

import { type Api, type FailureReason, PdfdietError } from "./core.mjs";

/**
 * The part of a `Worker`, `MessagePort` or worker global scope used here,
 * written out so the declarations need no DOM types.
 */
export interface Port {
  postMessage(message: unknown, transfer: ArrayBuffer[]): void;
  onmessage: ((event: { data: unknown }) => void) | null;
  onmessageerror: ((event: unknown) => void) | null;
}

/**
 * {@link Api} with every function run in a worker: same arguments, the
 * result as a promise.
 */
export type WorkerApi = {
  [K in keyof Api]: (...args: Parameters<Api[K]>) => Promise<Awaited<ReturnType<Api[K]>>>;
} & {
  /**
   * Settles when the worker has loaded the wasm. Rejects if it could not
   * start or load it; every call then rejects with the same error.
   * Awaiting it is optional: calls wait for it themselves.
   */
  readonly ready: Promise<void>;
  /**
   * Stop the worker at once. Calls still running or waiting, and any made
   * later, reject with `Error("pdfdiet: worker terminated")`. Calling it
   * again does nothing.
   */
  terminate(): void;
};

// Every function of Api, for the client to forward. The checks below fail
// to compile if Api gains or loses one.
const names = ["compress", "inspect"] as const;
type Assert<T extends true> = T;
type Name = (typeof names)[number];
type _Exhaustive = Assert<[keyof Api] extends [Name] ? ([Name] extends [keyof Api] ? true : false) : false>;

interface WireError {
  name: string;
  message: string;
  reason?: string;
}
type ClientMessage =
  | { type: "init"; wasm: unknown }
  | { type: "call"; id: number; fn: string; args: unknown[] };
type WorkerMessage =
  | { type: "ready" }
  | { type: "failed"; error: WireError }
  | { type: "result"; id: number; value: unknown }
  | { type: "error"; id: number; error: WireError };

// Errors are sent as plain data and rebuilt on the other side, which is
// the same in every browser (structured cloning of errors is not).
function toWire(e: unknown): WireError {
  try {
    if (e instanceof PdfdietError) return { name: e.name, message: e.message, reason: e.reason };
    if (e instanceof Error) return { name: String(e.name), message: String(e.message) };
    return { name: "Error", message: String(e) };
  } catch {
    return { name: "Error", message: "pdfdiet: unknown error" }; // String() itself threw
  }
}

function fromWire(w: WireError): Error {
  switch (w.name) {
    case "TypeError":
      return new TypeError(w.message);
    case "RangeError":
      return new RangeError(w.message);
    case "PdfdietError":
      return new PdfdietError(w.reason as FailureReason, w.message);
    default: {
      const e = new Error(w.message);
      // Keep the kind (e.g. a wasm RuntimeError) for the message, not the class.
      if (w.name !== "Error") e.name = w.name;
      return e;
    }
  }
}

// Byte arrays at the top level of a result that own their whole buffer
// (the output of compress) are moved, not copied.
function transferables(value: unknown): ArrayBuffer[] {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return [];
  const out: ArrayBuffer[] = [];
  for (const v of Object.values(value)) {
    if (
      v instanceof Uint8Array &&
      v.buffer instanceof ArrayBuffer &&
      v.byteOffset === 0 &&
      v.byteLength === v.buffer.byteLength &&
      !out.includes(v.buffer)
    ) {
      out.push(v.buffer);
    }
  }
  return out;
}

/**
 * A Uint8Array that is a view into part of a larger buffer, copied to its
 * own: structured cloning copies the whole underlying buffer otherwise.
 */
export function compact(v: unknown): unknown {
  // The constructor, not slice(): a Node Buffer's slice() (and a
  // subclass's) shares the memory instead of copying it.
  return v instanceof Uint8Array && (v.byteOffset !== 0 || v.byteLength !== v.buffer.byteLength)
    ? new Uint8Array(v)
    : v;
}

/**
 * Worker side: wait for the init message, load the wasm with `init`, then
 * answer calls one at a time, in the order they arrive.
 */
export function serve(port: Port, api: Api, init: (wasm: unknown) => Promise<void>): void {
  let ready: Promise<void> | undefined;
  let queue = Promise.resolve();
  port.onmessage = ({ data }) => {
    const msg = data as ClientMessage;
    if (msg.type === "init") {
      if (ready) return;
      // async, so that init throwing synchronously also becomes a rejection.
      ready = (async () => init(msg.wasm))();
      ready.then(
        () => port.postMessage({ type: "ready" } satisfies WorkerMessage, []),
        (e: unknown) => port.postMessage({ type: "failed", error: toWire(e) } satisfies WorkerMessage, []),
      ).catch(() => {});
      return;
    }
    if (msg.type !== "call") return;
    const { id, fn, args } = msg;
    const job = async () => {
      try {
        if (!ready) throw new Error("pdfdiet: the worker was not initialized");
        await ready;
        if (!(names as readonly string[]).includes(fn)) throw new TypeError(`pdfdiet: no function ${fn}`);
        const value: unknown = (api[fn as keyof Api] as (...a: unknown[]) => unknown)(...args);
        port.postMessage({ type: "result", id, value } satisfies WorkerMessage, transferables(value));
      } catch (e) {
        port.postMessage({ type: "error", id, error: toWire(e) } satisfies WorkerMessage, []);
      }
    };
    // Whatever happens to one call, the next still runs.
    queue = queue.then(job).catch(() => {});
  };
  // A message that cannot be read has no id to answer: fail everything.
  port.onmessageerror = () => {
    const error = { name: "Error", message: "pdfdiet: a message to the worker could not be read" };
    port.postMessage({ type: "failed", error } satisfies WorkerMessage, []);
  };
}

/**
 * Client side: send the init message, and return the API that forwards
 * calls over `port`. `stop` ends the worker; `fail` rejects everything
 * with an error found outside the protocol (such as the worker's `error`
 * event).
 */
export function connect(
  port: Port,
  wasm: unknown,
  stop: () => void,
): { api: WorkerApi; fail: (e: Error) => void } {
  const pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();
  let nextId = 0;
  let dead: Error | undefined;
  let settle!: { resolve: () => void; reject: (e: Error) => void };
  const ready = new Promise<void>((resolve, reject) => {
    settle = { resolve, reject };
  });
  // Calls report the failure too: not awaiting `ready` is not a mistake.
  ready.catch(() => {});

  function fail(e: Error): void {
    if (dead) return;
    dead = e;
    settle.reject(e);
    for (const p of pending.values()) p.reject(e);
    pending.clear();
  }

  // postMessage throws DataCloneError (a DOMException) for a function or
  // another value that cannot be copied to the worker.
  function sendError(e: unknown): Error {
    if (typeof e === "object" && e !== null && (e as { name?: unknown }).name === "DataCloneError") {
      return new TypeError("pdfdiet: arguments must be structured-cloneable");
    }
    return e instanceof Error ? e : new Error(String(e));
  }

  port.onmessage = ({ data }) => {
    const msg = data as WorkerMessage;
    switch (msg.type) {
      case "ready":
        settle.resolve();
        break;
      case "failed":
        fail(fromWire(msg.error));
        break;
      case "result":
      case "error": {
        const p = pending.get(msg.id);
        if (!p) return;
        pending.delete(msg.id);
        if (msg.type === "result") p.resolve(msg.value);
        else p.reject(fromWire(msg.error));
        break;
      }
    }
  };
  port.onmessageerror = () => fail(new Error("pdfdiet: a message from the worker could not be read"));

  function call(fn: string, args: unknown[]): Promise<unknown> {
    if (dead) return Promise.reject(dead);
    return new Promise((resolve, reject) => {
      const id = ++nextId;
      try {
        // A compacted copy is ours: move it rather than copy it again.
        const sent = args.map(compact);
        const moved = sent
          .filter((a, i) => a !== args[i] && (a as Uint8Array).buffer !== (args[i] as Uint8Array).buffer)
          .map((a) => (a as Uint8Array).buffer as ArrayBuffer);
        port.postMessage({ type: "call", id, fn, args: sent } satisfies ClientMessage, moved);
      } catch (e) {
        reject(sendError(e));
        return;
      }
      pending.set(id, { resolve, reject });
    });
  }

  let terminated = false;
  const api = {
    ready,
    terminate(): void {
      if (terminated) return;
      terminated = true;
      const e = new Error("pdfdiet: worker terminated");
      fail(e);
      dead = e; // also after an earlier failure: later calls say why
      stop();
    },
  } as Record<string, unknown>;
  for (const fn of names) api[fn] = (...args: unknown[]) => call(fn, args);

  try {
    port.postMessage({ type: "init", wasm } satisfies ClientMessage, []);
  } catch (e) {
    fail(sendError(e));
  }
  return { api: api as WorkerApi, fail };
}
