//! RC-20: `cargo build` no longer runs the Next.js export or materializes
//! `web/out/` for rust-embed. `web/package.json` stays as a version face.
//! `src/command/web_assets.rs` is a compile stub until RC-23 deletes the
//! Code SCC (`internal/ai/web`, `code.rs`, `graph.rs`, `mcp/`, `codex/`).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
