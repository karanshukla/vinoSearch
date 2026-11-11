//! The models and the device they run on: pinning and fetching them, the
//! OpenVINO runtime, text, image and audio embeddings, OCR, and the parity check
//! that a device runs them right.

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used once Kind::Media lands (#187)")
)]
pub mod audio;
pub mod embed;
pub mod model;
pub mod ocr;
pub mod parity;
pub mod runtime;
pub mod vision;
