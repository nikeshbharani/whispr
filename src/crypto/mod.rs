//! Cryptography for whispr.
//!
//! All confidentiality and authentication is provided by the
//! [Noise Protocol Framework](https://noiseprotocol.org/) via the [`noise`]
//! submodule. There is deliberately nothing else here: we lean on a small,
//! audited primitive set rather than rolling our own scheme.

pub mod noise;
