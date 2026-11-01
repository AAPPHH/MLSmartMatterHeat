#![no_std]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[forbid(unsafe_code)]
pub mod tlv;
#[forbid(unsafe_code)]
pub mod crypto;
#[forbid(unsafe_code)]
pub mod client;

#[cfg(feature = "ffi")]
mod ffi;
