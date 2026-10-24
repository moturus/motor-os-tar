//! The archive formats and platform support behind the `tar` and `zstd`
//! commands, which run on Motor OS and Linux and produce the same bytes on
//! both.

pub mod archive;
pub mod platform;
pub mod zst;
