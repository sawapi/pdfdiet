// The worker protocol, run over a MessageChannel in Node (which has no Web
// Workers): `serve` on one end with the browser build, `connect` on the
// other, as createWorker() wires them. Run from test.mjs, after the
// browser build has been loaded and checked.
import assert from "node:assert/strict";
import { MessageChannel } from "node:worker_threads";
import { connect, serve } from "./dist/protocol.mjs";

export async function testWorker(web, input) {
  // A pair of ports, the serving end calling `init` and `api` as given.
  function pair(api = web, init = (w) => web.init(w)) {
    const { port1, port2 } = new MessageChannel();
    serve(port2, api, init);
    const c = connect(port1, undefined, () => {
      port1.close();
      port2.close();
    });
    return c;
  }

  // Same results as calling the browser build directly; the input is
  // copied, not taken.
  {
    const w = pair();
    await w.api.ready;
    const before = input.byteLength;
    const [res, direct] = [await w.api.compress(input, { dpi: 72 }), web.compress(input, { dpi: 72 })];
    assert.equal(input.byteLength, before, "the caller's buffer is not detached");
    assert.ok(res.output instanceof Uint8Array);
    assert.deepEqual(Buffer.from(res.output), Buffer.from(direct.output));
    assert.deepEqual(res.log, direct.log);
    assert.equal(res.imagesTotal, direct.imagesTotal);
    assert.deepEqual(await w.api.inspect(input), web.inspect(input));

    // Errors keep their class, message and reason.
    await assert.rejects(w.api.compress(new Uint8Array([1, 2, 3])), (e) =>
      e instanceof web.PdfdietError && e.reason === "invalid-pdf" && e.message === getMessage(() => web.compress(new Uint8Array([1, 2, 3]))),
    );
    await assert.rejects(w.api.compress(input, { quality: 0 }), /^RangeError: pdfdiet: quality/);
    await assert.rejects(w.api.compress("in.pdf"), /^TypeError: pdfdiet: input/);
    // A function cannot be sent: a rejection, not a throw.
    const p = w.api.compress(input, { dpi: () => 150 });
    assert.ok(p instanceof Promise);
    await assert.rejects(p, /^TypeError: pdfdiet: arguments must be structured-cloneable$/);

    // One call at a time, answered in order.
    const order = [];
    await Promise.all([
      w.api.compress(input).then(() => order.push("compress")),
      w.api.inspect(input).then(() => order.push("inspect")),
      w.api.inspect(new Uint8Array(1)).catch(() => order.push("garbage")),
    ]);
    assert.deepEqual(order, ["compress", "inspect", "garbage"]);

    // terminate: pending and later calls reject; twice is fine.
    const running = w.api.compress(input);
    w.api.terminate();
    w.api.terminate();
    await assert.rejects(running, /^Error: pdfdiet: worker terminated$/);
    await assert.rejects(w.api.inspect(input), /^Error: pdfdiet: worker terminated$/);
  }

  // The output buffer is moved out of the worker, not copied.
  {
    let served;
    const w = pair({
      compress: (...a) => (served = web.compress(...a)),
      inspect: web.inspect,
    });
    const res = await w.api.compress(input);
    assert.equal(served.output.byteLength, 0, "transferred: detached on the serving side");
    assert.ok(res.output.byteLength > 0);
    w.api.terminate();
  }

  // A failed load rejects `ready`, calls already waiting, and later calls.
  {
    const w = pair(web, async () => {
      throw new WebAssembly.CompileError("bad wasm");
    });
    const waiting = w.api.inspect(input);
    await assert.rejects(w.api.ready, /^CompileError: bad wasm$/);
    await assert.rejects(waiting, /bad wasm/);
    await assert.rejects(w.api.compress(input), /bad wasm/);
    w.api.terminate();
    await assert.rejects(w.api.compress(input), /worker terminated/);
  }

  // An error from outside the protocol (the worker's error event).
  {
    const w = pair(web, () => new Promise(() => {})); // never loads
    const waiting = w.api.compress(input);
    w.fail(new Error("pdfdiet: the worker failed to start"));
    await assert.rejects(w.api.ready, /failed to start/);
    await assert.rejects(waiting, /failed to start/);
    w.api.terminate();
  }

  // A thrown value that cannot even be turned into a string still gets an
  // answer, and the calls after it still run.
  {
    const evil = { toString() { throw new Error("no"); } };
    const w = pair({ compress: () => { throw evil; }, inspect: web.inspect });
    await assert.rejects(w.api.compress(input), /^Error: pdfdiet: unknown error$/);
    assert.equal((await w.api.inspect(input)).length, 1);
    w.api.terminate();
  }

  // A message the worker cannot read fails everything, not just one call.
  {
    const sent = [];
    const fake = { postMessage: (m) => sent.push(m), onmessage: null, onmessageerror: null };
    serve(fake, web, (w) => web.init(w));
    fake.onmessageerror({});
    assert.deepEqual(sent, [
      { type: "failed", error: { name: "Error", message: "pdfdiet: a message to the worker could not be read" } },
    ]);
  }

  // A view into part of a buffer is sent as its own copy, not the whole buffer.
  {
    const sent = [];
    const moved = [];
    const fake = { postMessage: (m, t) => (sent.push(m), moved.push(t)), onmessage: null, onmessageerror: null };
    const c = connect(fake, undefined, () => {});
    const big = new Uint8Array(1 << 20);
    const calls = [c.api.inspect(big.subarray(10, 20)), c.api.inspect(big)];
    assert.equal(sent[1].args[0].buffer.byteLength, 10, "a subarray is compacted");
    assert.equal(sent[2].args[0], big, "a whole buffer is sent as is");
    assert.deepEqual(moved.map((t) => t.length), [0, 1, 0], "only the compacted copy is transferred");
    assert.equal(big.byteLength, 1 << 20, "the caller's buffer is not detached");
    for (const call of calls) call.catch(() => {});
    c.api.terminate();
    for (const call of calls) await assert.rejects(call, /worker terminated/);
  }

  // Node Buffers, whose slice() shares memory: a pooled one and a
  // subarray are sent as real copies, over a real channel, and the
  // caller's memory stays attached.
  {
    const w = pair();
    const pooled = Buffer.from([1, 2, 3]);
    assert.ok(pooled.byteLength < pooled.buffer.byteLength, "a pooled Buffer");
    await assert.rejects(w.api.inspect(pooled), (e) => e.reason === "invalid-pdf");
    assert.deepEqual([...pooled], [1, 2, 3]);
    const host = Buffer.concat([Buffer.alloc(100), input, Buffer.alloc(100)]);
    const view = host.subarray(100, 100 + input.length);
    assert.equal((await w.api.inspect(view)).length, 1);
    assert.equal(host.byteLength, input.length + 200, "not detached");
    class Odd extends Uint8Array {
      slice() {
        return this; // a subclass whose slice() does not copy
      }
    }
    const odd = new Odd(host.buffer, host.byteOffset + 100, input.length);
    assert.equal((await w.api.inspect(odd)).length, 1);
    assert.equal(host.byteLength, input.length + 200);
    w.api.terminate();
  }

  // The entry point itself: Node has no Web Workers.
  const entry = await import("@sawapi/pdfdiet/worker");
  assert.equal(entry.PdfdietError, web.PdfdietError, "one error class for every entry point");
  assert.throws(() => entry.createWorker(), /^TypeError: pdfdiet: createWorker needs Web Workers$/);
  assert.deepEqual(Object.keys(entry).sort(), ["PdfdietError", "createWorker"]);
  console.log("worker protocol: ok");
}

function getMessage(f) {
  try {
    f();
  } catch (e) {
    return e.message;
  }
}
