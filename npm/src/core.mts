// The typed API shared by the Node and browser entry points. The
// wasm-bindgen glue is an implementation detail: its result classes hold
// wasm memory, so they are copied into plain objects here and freed.

/** Options for {@link compress}. Every field is optional; `undefined` means the default. */
export interface CompressOptions {
  /** Target effective DPI: images drawn at a higher resolution are downsampled. Default 150. */
  dpi?: number | undefined;
  /** JPEG quality for re-encoded images, an integer from 1 to 100. Default 80. */
  quality?: number | undefined;
  /**
   * Process a document protected only by an owner password (no-print,
   * no-copy and similar restrictions), removing those restrictions.
   * Without it such documents are refused. Default false.
   */
  stripProtection?: boolean | undefined;
  /**
   * Process a digitally signed document, which invalidates its signatures.
   * Without it signed documents are refused. Default false.
   */
  breakSignatures?: boolean | undefined;
  /**
   * Also drop the outlines of embedded glyphs (TrueType, and CID-keyed CFF)
   * that no text in the document uses. Smaller, but the fonts can no longer
   * show text added later. Default false.
   */
  subsetOutlines?: boolean | undefined;
  /** Remove EXIF (GPS, camera, dates), XMP and other metadata from embedded JPEGs, losslessly. Default false. */
  stripJpegMetadata?: boolean | undefined;
}

/** What {@link compress} returns. */
export interface CompressResult {
  /** The new PDF (its own `ArrayBuffer`, so it can be transferred or put in a `Blob`). */
  output: Uint8Array<ArrayBuffer>;
  /** What was changed, and what was left alone and why, one entry per line. */
  log: string[];
  /** Image XObjects found in the document. */
  imagesTotal: number;
  /** Images downsampled, re-encoded or losslessly re-optimized. */
  imagesRecompressed: number;
}

/**
 * Why a document was not processed:
 * - `"invalid-pdf"`: it could not be parsed as a PDF.
 * - `"encrypted"`: it is password-protected and cannot be read.
 * - `"protected"`: it has an owner password; see {@link CompressOptions.stripProtection}.
 * - `"signed"`: it is digitally signed; see {@link CompressOptions.breakSignatures}.
 * - `"empty"`: it has no readable pages.
 */
export type FailureReason = "invalid-pdf" | "encrypted" | "protected" | "signed" | "empty";

/** Thrown by {@link compress} and {@link inspect} when a document is not processed. */
export class PdfdietError extends Error {
  readonly reason: FailureReason;

  constructor(reason: FailureReason, message: string) {
    super(message);
    this.name = "PdfdietError";
    this.reason = reason;
  }
}

/** One image in the document, as listed by {@link inspect}. */
export interface ImageInfo {
  /** PDF object number and generation. */
  object: [objectNumber: number, generation: number];
  /** Size in pixels. */
  width: number;
  height: number;
  bitsPerComponent: number;
  /** Color space, e.g. `"DeviceRGB"`, `"ICCBased(N=3)"`, `"Indexed"`. */
  colorSpace: string;
  /** Color components per pixel, or null when not known. */
  components: number | null;
  /** Stream filters in order, e.g. `["DCTDecode"]`. */
  filters: string[];
  /** Encoded size of the image stream in bytes. */
  bytes: number;
  /** Largest size the image is drawn at on any page, in inches; null if it is never drawn. */
  drawnSize: { width: number; height: number } | null;
  /** Effective resolution at that size; null if it is never drawn (or drawn vanishingly small). */
  dpi: { x: number; y: number } | null;
}

// The parts of the wasm-bindgen glue used here, written out so the
// published declarations do not depend on the glue's. The entry points
// pass the real glue in (and check the parameter lists both ways), so a
// mismatch fails to compile there.
interface Freeable {
  free: () => void;
}
interface FailureObject extends Freeable {
  readonly reason: string;
  readonly message: string;
}
// Function-typed properties rather than methods, so parameters are compared
// strictly (methods are compared bivariantly).
export interface Bindings {
  compress_pdf: (
    input: Uint8Array,
    dpi: number,
    quality: number,
    strip_protection: boolean,
    break_signatures: boolean,
    subset_outlines: boolean,
    strip_jpeg_metadata: boolean,
  ) => Freeable & {
    take_output: () => Uint8Array;
    readonly log: string[];
    readonly images_total: number;
    readonly images_recompressed: number;
  };
  inspect_images: (input: Uint8Array) => (Freeable & {
    readonly object_number: number;
    readonly generation: number;
    readonly width: number;
    readonly height: number;
    readonly bits_per_component: number;
    readonly color_space: string;
    readonly components: number | undefined;
    readonly filters: string[];
    readonly bytes: number;
    readonly width_inch: number | undefined;
    readonly height_inch: number | undefined;
    readonly dpi_x: number | undefined;
    readonly dpi_y: number | undefined;
  })[];
  // A class with a private constructor: only used with instanceof.
  Failure: Function & { readonly prototype: FailureObject };
}

// For error messages: NaN, Infinity, bigints and symbols shown as they are.
function show(v: unknown): string {
  if (typeof v === "string") return JSON.stringify(v);
  try {
    return String(v);
  } catch {
    return Object.prototype.toString.call(v); // e.g. a null-prototype object
  }
}

const reasons: readonly string[] = ["invalid-pdf", "encrypted", "protected", "signed", "empty"];

/** The functions both entry points export, with their documentation. */
export interface Api {
  /**
   * Shrink a PDF: downsample and re-encode images drawn above the target
   * resolution, and optimize fonts and structure losslessly.
   *
   * @param input The PDF file's bytes (a Node `Buffer` is fine).
   * @throws {PdfdietError} when the document is not processed.
   * @throws {TypeError} for an argument of the wrong type.
   * @throws {RangeError} for an out-of-range `dpi` or `quality`.
   */
  compress(input: Uint8Array, options?: CompressOptions): CompressResult;
  /**
   * List the images in a PDF, with the largest size each is drawn at and
   * the resulting effective resolution.
   *
   * @throws {PdfdietError} when the input cannot be read as a PDF.
   * @throws {TypeError} when `input` is not a `Uint8Array`.
   */
  inspect(input: Uint8Array): ImageInfo[];
}

// Compile-time checks used by the entry points against the real glue: the
// parameter lists must match exactly. Passing the glue to makeApi alone
// would miss a parameter the glue dropped (fewer parameters is assignable).
type Same<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;
export type ParamsMatch<G extends Pick<Bindings, "compress_pdf" | "inspect_images">> = [
  Same<Parameters<G["compress_pdf"]>, Parameters<Bindings["compress_pdf"]>>,
  Same<Parameters<G["inspect_images"]>, Parameters<Bindings["inspect_images"]>>,
];

export function makeApi(glue: Bindings, ensureReady: () => void = () => {}): Api {
  // Call into wasm, turning a thrown Failure into a PdfdietError.
  function call<T>(f: () => T): T {
    try {
      return f();
    } catch (e) {
      if (!(e instanceof glue.Failure)) throw e;
      const failure = e as FailureObject;
      let reason: string;
      let message: string;
      try {
        reason = failure.reason;
        message = failure.message;
      } finally {
        failure.free();
      }
      throw new PdfdietError(
        (reasons.includes(reason) ? reason : "invalid-pdf") as FailureReason,
        message,
      );
    }
  }

  function bytes(input: Uint8Array): Uint8Array {
    if (!(input instanceof Uint8Array)) {
      throw new TypeError("pdfdiet: input must be a Uint8Array (a Node Buffer is fine)");
    }
    return input;
  }

  function flag(options: CompressOptions, name: keyof CompressOptions): boolean {
    const v = options[name];
    if (v === undefined) return false;
    if (typeof v !== "boolean") {
      throw new TypeError(`pdfdiet: ${name} must be a boolean, got ${show(v)}`);
    }
    return v;
  }

  function compress(input: Uint8Array, options?: CompressOptions): CompressResult {
    ensureReady();
    const pdf = bytes(input);
    const opts = options ?? {};
    if (typeof opts !== "object" || Array.isArray(opts)) {
      throw new TypeError("pdfdiet: options must be an object");
    }
    const { dpi = 150, quality = 80 } = opts;
    // A wrong type is a TypeError, a number out of range a RangeError.
    if (typeof dpi !== "number") throw new TypeError(`pdfdiet: dpi must be a number, got ${show(dpi)}`);
    if (!(Number.isFinite(dpi) && dpi > 0)) {
      throw new RangeError(`pdfdiet: dpi must be a positive number, got ${show(dpi)}`);
    }
    if (typeof quality !== "number") {
      throw new TypeError(`pdfdiet: quality must be a number, got ${show(quality)}`);
    }
    if (!(Number.isInteger(quality) && quality >= 1 && quality <= 100)) {
      throw new RangeError(`pdfdiet: quality must be an integer from 1 to 100, got ${show(quality)}`);
    }
    const flags = [
      flag(opts, "stripProtection"),
      flag(opts, "breakSignatures"),
      flag(opts, "subsetOutlines"),
      flag(opts, "stripJpegMetadata"),
    ] as const;
    const res = call(() => glue.compress_pdf(pdf, dpi, quality, ...flags));
    try {
      return {
        // The glue returns a fresh copy out of wasm memory.
        output: res.take_output() as Uint8Array<ArrayBuffer>,
        log: res.log,
        imagesTotal: res.images_total,
        imagesRecompressed: res.images_recompressed,
      };
    } finally {
      res.free();
    }
  }

  function inspect(input: Uint8Array): ImageInfo[] {
    ensureReady();
    const entries = call(() => glue.inspect_images(bytes(input)));
    try {
      return entries.map((e) => {
        const w = e.width_inch;
        const h = e.height_inch;
        const dx = e.dpi_x;
        const dy = e.dpi_y;
        return {
          object: [e.object_number, e.generation],
          width: e.width,
          height: e.height,
          bitsPerComponent: e.bits_per_component,
          colorSpace: e.color_space,
          components: e.components ?? null,
          filters: e.filters,
          bytes: e.bytes,
          drawnSize: w !== undefined && h !== undefined ? { width: w, height: h } : null,
          dpi: dx !== undefined && dy !== undefined ? { x: dx, y: dy } : null,
        };
      });
    } finally {
      // Every entry, even if converting one of them threw.
      for (const e of entries) e.free();
    }
  }

  return { compress, inspect };
}
