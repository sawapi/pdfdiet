// Type test, DOM part: compiled only by test-types.tsconfig.json (which
// has the DOM lib). The output goes straight into a Blob, which needs the
// output typed Uint8Array<ArrayBuffer> (TypeScript 5.7 or later).

import { compress } from "@sawapi/pdfdiet/web";

declare const bytes: Uint8Array;

const blob = new Blob([compress(bytes).output], { type: "application/pdf" });
void blob;
