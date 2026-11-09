//! OwnHealth core: configuration, database, blobs, jobs and the domain services every lane uses.
//! Ported from 0.1.1 `ownhealth/{config,blobs,runtime,db,jobs,core,state/canonical}.py`.

pub mod acceptance;
pub mod access;
pub mod arrive;
pub mod audit;
pub mod blobs;
pub mod canonical;
pub mod config;
pub mod dedup;
pub mod demo;
pub mod demo_pdf;
pub mod documents;
pub mod error;
pub mod fhir;
pub mod ids;
pub mod ingest;
pub mod jobs;
pub mod lifecycle;
pub mod migrate;
pub mod net;
pub mod observations;
pub mod provenance;
pub mod pyjson;
pub mod pyrandom;
pub mod quarantine;
pub mod records;
pub mod review;
pub mod runtime;
pub mod secrets;
pub mod sources;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod text;

pub use error::{Error, Result};
pub use runtime::Runtime;
