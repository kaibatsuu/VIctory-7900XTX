//! Xbox Series shaders to host shaders.
//!
//! An Xbox Series shader is a standard DXBC container ([`dxbc`]). Its `DXIL`
//! part ([`dxil`]) may carry Xbox additions around the LLVM bitcode: an
//! `XDAT` table before it indexing blobs after it, among them `xbop`
//! (compiler options) and the zlib-compressed GFX10 code. The bitcode may use
//! console-only intrinsics, so the GFX10 code is translated in general.

pub mod bvh;
pub mod dxbc;
pub mod dxil;
pub mod emit;
pub mod gfx10;
pub mod io;
pub mod ir;
pub mod native;
pub mod resource;
pub mod spill;
pub mod structure;

/// The version of what the translator makes, part of the key of every
/// cached translation (`victory-gpu`'s `shader_cache`): **bump it with any
/// change that changes the translation of a shader** (its DXIL or what the
/// host binds for it). Changes that do not (speed, diagnostics, tests,
/// shaders that did not translate before) leave it, and every shader
/// translated before is kept across the rebuild. `victory shaders`
/// translates again what an earlier build of the translator cached and says
/// how many came out different: if any did, the bump was due.
pub const TRANSLATOR_VERSION: u32 = 1;

pub use dxbc::{Container, FourCC, Part};
pub use dxil::{DxilProgram, ShaderKind, XboxExtras};
pub use native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource, native_programs,
};

/// Whether a console program can store to buffer memory (the first) and
/// through raw addresses (the second), from one decode of its code. The
/// translator's `stores` and `stores_by_address`, fused so a shader cache hit
/// decodes once, or not at all.
pub fn analyze_stores(program: &NativeProgram) -> (bool, bool) {
    use gfx10::{Encoding, decode};
    let Ok(code) = decode(&program.code) else {
        // Code that does not decode is taken as storing both ways.
        return (true, true);
    };
    let mut stores = false;
    let mut by_address = false;
    for i in code.iter() {
        let name = i.name();
        match i.encoding {
            Encoding::Mubuf | Encoding::Mtbuf | Encoding::Smem | Encoding::Flat | Encoding::Global => {
                if name.is_none_or(|n| (n.contains("store") || n.contains("atomic")) && !n.contains("scratch")) {
                    stores = true;
                    if matches!(i.encoding, Encoding::Flat | Encoding::Global) {
                        by_address = true;
                    }
                }
            }
            Encoding::Sop1 => {
                if name.is_none_or(|n| matches!(n, "s_setpc_b64" | "s_swappc_b64")) {
                    stores = true;
                }
            }
            _ => {}
        }
        if stores && by_address {
            break;
        }
    }
    (stores, by_address)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShaderError {
    #[error("not a DXBC container")]
    NotDxbc,
    #[error("malformed shader: {0}")]
    Malformed(&'static str),
    #[error("GFX10 code at {offset:#x}: {reason}")]
    Isa { offset: usize, reason: &'static str },
    /// Valid code the translator cannot handle yet: an opcode name or a construct.
    #[error("GFX10 code at {offset:#x}: {what} is not supported")]
    Unsupported { offset: usize, what: &'static str },
}
