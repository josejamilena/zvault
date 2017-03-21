mod error;
mod writer;
mod bundle;
mod db;

pub use self::error::BundleError;
pub use self::writer::BundleWriter;
pub use self::bundle::*;
pub use self::db::*;

pub static HEADER_STRING: [u8; 7] = *b"zvault\x01";
pub static HEADER_VERSION: u8 = 1;


/*

Bundle format
- Magic header + version
- Encoded header structure (contains size of next structure)
- Encoded chunk list (with chunk hashes and sizes)
- Chunk data

*/
