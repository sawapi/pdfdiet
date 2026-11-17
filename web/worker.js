// Runs pdfdiet off the main thread so the page stays responsive while a
// large PDF is processed. One job at a time; the page terminates and
// replaces the worker to cancel one.
import { init, compress } from "./pkg/dist/web.mjs";

const ready = init();

self.onmessage = async ({ data: job }) => {
  try {
    await ready;
    const t0 = performance.now();
    // Protection and signatures are never stripped on the page.
    const res = compress(new Uint8Array(job.input), {
      dpi: job.dpi,
      quality: job.quality,
      subsetOutlines: job.subsetOutlines,
      stripJpegMetadata: job.stripJpegMetadata,
    });
    const reply = {
      id: job.id,
      ok: true,
      output: res.output.buffer,
      log: res.log.join("\n"),
      imagesTotal: res.imagesTotal,
      imagesRecompressed: res.imagesRecompressed,
      ms: performance.now() - t0,
    };
    self.postMessage(reply, [res.output.buffer]);
  } catch (err) {
    self.postMessage({
      id: job.id,
      ok: false,
      reason: err?.reason, // set on PdfdietError: why the document was not processed
      error: String(err && err.message ? err.message : err),
    });
  }
};

ready.then(
  () => self.postMessage({ ready: true }),
  (err) => self.postMessage({ ready: false, error: String(err) }),
);
