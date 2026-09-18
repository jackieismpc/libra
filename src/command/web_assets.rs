//! Stub for the retired Next.js embed.
//!
//! RC-20 stopped `build.rs` from exporting `web/out/` and rust-embed no
//! longer ships those bytes. Callers in `internal/ai/web` still compile
//! against `WebAssets::get` until RC-23 deletes that SCC. Every lookup
//! returns `None`.

use std::borrow::Cow;

/// Former rust-embed payload. Kept so existing `content.data` reads compile.
pub struct EmbeddedFile {
    pub data: Cow<'static, [u8]>,
}

pub struct WebAssets;

impl WebAssets {
    pub fn get(_path: &str) -> Option<EmbeddedFile> {
        None
    }
}
