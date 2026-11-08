//! The synthetic lab-slip fixtures in Rust (`fixtures/labslips`): generator, stand-in normalizer,
//! matcher and evaluator. Shared by the integration tests and `examples/labslips.rs`.
#![allow(dead_code)]

pub mod catalog;
pub mod env;
pub mod evaluate;
pub mod fake_normalize;
pub mod generate;
pub mod matching;
pub mod pdf;
