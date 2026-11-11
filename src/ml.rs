//! The models and the device they run on: pinning and fetching them, the
//! OpenVINO runtime, text, image and audio embeddings, OCR, and the parity check
//! that a device runs them right.

pub mod audio;
pub mod embed;
pub mod model;
pub mod ocr;
pub mod parity;
pub mod runtime;
pub mod vision;
