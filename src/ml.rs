//! The models and the device they run on: pinning and fetching them, the
//! OpenVINO runtime, text and image embeddings, OCR, and the parity check
//! that a device runs them right.

pub mod embed;
pub mod model;
pub mod ocr;
pub mod parity;
pub mod runtime;
pub mod vision;
