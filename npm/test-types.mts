// Type test: compiled with `tsc -p test-types.tsconfig.json` (no output) to
// check the published declarations read the way the README says. Lines
// marked @ts-expect-error must fail to type-check.

import initDefault, { compress, inspect, init, initSync, PdfdietError } from "@sawapi/pdfdiet";
import type { CompressOptions, CompressResult, FailureReason, ImageInfo } from "@sawapi/pdfdiet";
import * as web from "@sawapi/pdfdiet/web";

declare const bytes: Uint8Array;

await init();
// Browser-style calls type-check against either entry point.
await init("https://example.com/pdfdiet_wasm_bg.wasm");
await initDefault();
initSync(new Uint8Array(0));
const res: CompressResult = compress(bytes, { dpi: 150, quality: 80, subsetOutlines: true });
const output: Uint8Array<ArrayBuffer> = res.output;
const lines: string[] = res.log;
const counts: number = res.imagesTotal + res.imagesRecompressed;
compress(bytes);
const opts: CompressOptions = {};
compress(bytes, opts);

const images: ImageInfo[] = inspect(bytes);
for (const img of images) {
  const [num, gen]: [number, number] = img.object;
  const dpi: number | undefined = img.dpi?.x;
  const filters: string[] = img.filters;
  const components: number | null = img.components;
  void [num, gen, dpi, filters, components];
}

try {
  compress(bytes);
} catch (e) {
  if (e instanceof PdfdietError) {
    const reason: FailureReason = e.reason;
    switch (reason) {
      case "invalid-pdf":
      case "encrypted":
      case "protected":
      case "signed":
      case "empty":
        break;
      default: {
        const never: never = reason;
        void never;
      }
    }
  }
}

await web.init();
await web.init("https://example.com/pdfdiet_wasm_bg.wasm");
await web.init(new Uint8Array(0));
await web.init({ module_or_path: "https://example.com/pdfdiet_wasm_bg.wasm" });
await web.default();
web.initSync(new Uint8Array(0));
web.initSync({ module: new Uint8Array(0) });
web.compress(bytes, { stripJpegMetadata: true });
const webError: typeof PdfdietError = web.PdfdietError;

// @ts-expect-error: options are an object, not positional arguments
compress(bytes, 150, 80);
// @ts-expect-error: unknown option
compress(bytes, { dpiTarget: 150 });
// @ts-expect-error: no wasm memory handle on results
res.free();
// @ts-expect-error: the result is not a wasm class
res.images_total;
// @ts-expect-error: flags are booleans
compress(bytes, { subsetOutlines: "yes" });
// @ts-expect-error: not a reason
const bad: FailureReason = "corrupt";
// @ts-expect-error: the bindings are not part of the API
import { compress_pdf } from "@sawapi/pdfdiet";

void [output, lines, counts, webError, bad, compress_pdf];
