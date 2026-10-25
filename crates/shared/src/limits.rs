//! How big an image is read, by the indexer and by the search window's
//! thumbnails alike.

/// Images past this are skipped: decoding takes several times the file in memory.
pub const MAX_IMAGE_BYTES: u64 = 64 << 20;

/// Decoding a huge image can take far more memory than the file. Refused
/// past this many bytes held at once: the decoded pixels, a copy if its
/// orientation turns it, and the 8-bit RGB copy (a 100-megapixel RGB photo
/// is 3 x 300 MB).
pub const MAX_DECODED_BYTES: u64 = 1 << 30;
