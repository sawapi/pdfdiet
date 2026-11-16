// Runs pdfdiet off the main thread so the page stays responsive while a
// large PDF is processed. One job at a time; the page terminates and
// replaces the worker to cancel one.
import init, { compress_pdf } from "./pdfdiet_wasm.js";

const ready = init({ module_or_path: new URL("./pdfdiet_wasm_bg.wasm", import.meta.url) });

self.onmessage = async ({ data: job }) => {
  try {
    await ready;
    const t0 = performance.now();
    const res = compress_pdf(
      new Uint8Array(job.input),
      job.dpi,
      job.quality,
      false, // strip_protection: never on the page
      false, // break_signatures: never on the page
      job.subsetOutlines,
      job.stripJpegMetadata,
    );
    const output = res.output;
    const reply = {
      id: job.id,
      ok: true,
      output: output.buffer,
      log: res.log,
      imagesTotal: res.images_total,
      imagesRecompressed: res.images_recompressed,
      ms: performance.now() - t0,
    };
    res.free();
    self.postMessage(reply, [output.buffer]);
  } catch (err) {
    self.postMessage({ id: job.id, ok: false, error: String(err && err.message ? err.message : err) });
  }
};

ready.then(
  () => self.postMessage({ ready: true }),
  (err) => self.postMessage({ ready: false, error: String(err) }),
);
