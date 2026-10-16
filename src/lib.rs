//! snugly as a library: the archive format, compression pipeline, encryption, snapshots and
//! recovery records. The `snugly` binary is a thin command-line wrapper around these modules.

pub mod archive;
pub mod codec;
pub mod create;
pub mod crypto;
pub mod extract;
pub mod format;
pub mod recomp;
pub mod recovery;
pub mod splice;
pub mod stats;
