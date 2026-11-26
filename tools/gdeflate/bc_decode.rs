use std::sync::OnceLock;
use victory_formats::bcpack::{Tables, decode};

/// Inputs are disjoint buffers owned by the offline converter for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fx1_decode_bc(bc: u8, src: *const u8, n: usize, dst: *mut u8, len: usize) -> i32 {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    if src.is_null() || dst.is_null() || n == 0 || len == 0 { return -1; }
    std::panic::catch_unwind(|| {
        let input = unsafe { std::slice::from_raw_parts(src, n) };
        let output = unsafe { std::slice::from_raw_parts_mut(dst, len) };
        let tables = TABLES.get_or_init(|| Tables::builtin().expect("built-in BCPack tables"));
        if decode(bc, input, output, tables).is_ok() { 0 } else { -1 }
    }).unwrap_or(-1)
}
