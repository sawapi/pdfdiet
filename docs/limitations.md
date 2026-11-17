# Limitations

Images are re-encoded only when pdfdiet can decode them exactly. These are
left as they are:

- Encodings other than JPEG, zlib (`FlateDecode`) and uncompressed:
  JPEG 2000 (`JPXDecode`), JBIG2, LZW, run-length, ASCII-encoded, and
  images that are already CCITT fax-coded.
- Image masks (1-bit stencils drawn in the current color).
- Color spaces other than gray, RGB and CMYK (with or without an ICC
  profile), such as Lab, Separation or DeviceN, including palettes over
  them.
- Bit depths other than 8 bits per component, except 1-bit (bilevel)
  images and palettes of 1, 2, 4 or 8 bits.
- Images with a TIFF predictor, a color-key mask, or a non-default
  `/Decode` array. PNG predictors that match the image, a default
  `/Decode` and empty `/DecodeParms` are understood.
- Images wider or taller than 65,535 pixels or above 100 million pixels,
  and JPEGs whose header disagrees with the PDF's description of them.
- Font programs that cannot be parsed completely. And if any page's text
  cannot be followed (content that cannot be interpreted), glyph
  subsetting is skipped for the whole document, since a glyph that looks
  unused might not be.

Streams that pdfdiet does not otherwise change keep their content, but
uncompressed ones are compressed (losslessly, with zlib) when the file is
written, so their bytes differ.

Documents that are refused outright (encrypted, owner-password protected,
signed) are listed in [Safety](safety.md#refused-documents).

A file that is mostly vector graphics and already-compressed text, such
as sheet music or a typeset document without pictures, may hardly shrink:
there are no images to resample, and its streams are already compressed.
