//! The command line: the clap `Cli` struct, the string→enum value parsers,
//! the op-stream assembly (`Op`, `RawOps`, verb/pick parsing) and the Stage H
//! parity tests.

pub mod args;
pub mod command;
pub mod ops;
pub mod parsers;

#[cfg(test)]
mod tests;

pub use args::*;
pub use command::*;
pub use ops::*;
pub use parsers::*;
