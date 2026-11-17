// Node entry point: the wasm is loaded synchronously when this module is
// imported, so there is nothing to initialize.

import * as glue from "../node/pdfdiet_wasm.js";
import { type Api, makeApi, type ParamsMatch } from "./core.mjs";

// Fails to compile if the glue's parameters drift from what core expects.
type Assert<T extends [true, true]> = T;
type _Check = Assert<ParamsMatch<typeof glue>>;

export type { CompressOptions, CompressResult, FailureReason, ImageInfo } from "./core.mjs";
export { PdfdietError } from "./core.mjs";

const api = makeApi(glue);

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

/**
 * Does nothing in Node, where the wasm loads with this module. Present, with
 * the browser build's signature, so code written for the browser (`await
 * init(url)`, `initSync(bytes)`, `import init from ...`) also runs under
 * Node, as in server-side rendering or tests. The argument is ignored.
 */
export async function init(_wasm?: unknown): Promise<void> {}

/** Does nothing in Node; see {@link init}. The argument is ignored. */
export function initSync(_wasm?: unknown): void {}

export default init;
