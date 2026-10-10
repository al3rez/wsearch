//! Whole-disk file search for Windows: NTFS MFT crawl, USN live updates,
//! fuzzy names, trigram content grep.

pub mod content;
mod engine;
pub mod index;
pub mod live;
pub mod nt;
pub mod pathwin;
pub mod query;
pub mod server;
pub mod walk;
mod winpriv;

pub use content::{FileMatches, Grep, GrepResult};
pub use engine::{default_dir, Engine, Found, Options, Status};
pub use query::{GrepMode, Query};
pub use server::{connect, serve};
