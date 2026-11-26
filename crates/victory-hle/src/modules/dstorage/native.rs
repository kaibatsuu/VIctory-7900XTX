//! Optional native DirectStorage path for verified offline GDeflate packs.
//!
//! Dormant unless `VICTORY_GDEFLATE_PACK` names a locally built, verified
//! `FX1GDF01` pack and `fx1-storage.dll` (plus the signed `dstorage*.dll`
//! redistributables) sit beside it. Every request falls back to the original
//! zlib/BCPack CPU decoder otherwise, so this module is inert when the bridge
//! or pack is absent.
use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

use super::super::xgameruntime::HRESULT;
use super::Request;

/// `fx1_read_gdeflate`: `(device, pack_wpath, source, stored, decoded, flags,
/// dst, support) -> status`. Status `1` = decoded and delivered, `0` = the
/// bridge declined (index miss, unsupported, oversized lane), `< 0` = an
/// `HRESULT` the caller should surface.
type Read = unsafe extern "C" fn(
    *mut c_void,
    *const u16,
    *const u8,
    u32,
    u32,
    u32,
    *mut u8,
    *mut u32,
) -> i32;

struct Bridge {
    pack: Vec<u16>,
    read: Read,
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn bridge() -> Option<&'static Bridge> {
    static BRIDGE: OnceLock<Option<Bridge>> = OnceLock::new();
    BRIDGE
        .get_or_init(|| {
            let path = std::env::var_os("VICTORY_GDEFLATE_PACK")?;
            load_bridge(PathBuf::from(path))
        })
        .as_ref()
}

fn load_bridge(path: PathBuf) -> Option<Bridge> {
    let pack = path.canonicalize().ok()?;
    let dir = match std::env::var_os("VICTORY_GDEFLATE_BRIDGE_DIR") {
        Some(path) => PathBuf::from(path).canonicalize().ok()?,
        None => pack.parent()?.to_owned(),
    };
    let dll = wide(dir.join("fx1-storage.dll").as_os_str());
    let core = wide(dir.join("dstoragecore.dll").as_os_str());
    let search = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32;
    // The signed DirectStorage redistributables load before the local bridge.
    // SAFETY: a NUL-terminated wide path; a null handle is handled below.
    if unsafe { LoadLibraryExW(core.as_ptr(), std::ptr::null_mut(), search) }.is_null() {
        tracing::warn!(error = %std::io::Error::last_os_error(), "fx1 DirectStorage core unavailable");
        return None;
    }
    // SAFETY: as above.
    let handle = unsafe {
        LoadLibraryExW(
            dll.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if handle.is_null() {
        tracing::warn!(error = %std::io::Error::last_os_error(), "fx1 native storage unavailable");
        return None;
    }
    // SAFETY: this exact ABI is exported by tools/gdeflate/runtime.cpp. The
    // handle lives for the process lifetime.
    let symbol = unsafe { GetProcAddress(handle, c"fx1_read_gdeflate".as_ptr().cast()) }?;
    // SAFETY: the symbol matches the `Read` ABI above.
    let read: Read = unsafe { std::mem::transmute(symbol) };
    Some(Bridge { pack: wide(pack.as_os_str()), read })
}

/// Whether a request can use the native path: a file source with a decoded
/// size that fits the bridge's one 4 MiB lane.
fn eligible(request: &Request) -> bool {
    request.options & super::MEMORY_SOURCE == 0
        && request.destination_size != 0
        && request.destination_size <= 4 << 20
}

/// Decodes `request`'s block from the verified pack into a scratch buffer.
/// `Ok(Some(bytes))` means the GPU delivered and verified it; `Ok(None)` means
/// the caller should keep its CPU decoder; `Err(hr)` is a real failure.
pub(super) fn serve(request: &Request, source: &[u8], flags: u32) -> Result<Option<Vec<u8>>, HRESULT> {
    if !eligible(request) {
        return Ok(None);
    }
    let Some(bridge) = bridge() else {
        return Ok(None);
    };
    let Some(device) = victory_gpu::storage_device() else {
        return Ok(None);
    };
    let size = request.destination_size as usize;
    let mut out = vec![0u8; size];
    let mut support = 0u32;
    // SAFETY: `out` holds `size` bytes and `source` holds `source_size` bytes
    // for this call; the bridge reads only the pack file it opened itself.
    let status = unsafe {
        (bridge.read)(
            device,
            bridge.pack.as_ptr(),
            source.as_ptr(),
            request.source_size,
            request.destination_size,
            flags,
            out.as_mut_ptr(),
            &mut support,
        )
    };
    if status == 1 {
        return Ok(Some(out));
    }
    if status < 0 {
        tracing::warn!(
            hr = format_args!("{:#010x}", status as u32),
            "game GDeflate bridge failed; restoring original decoder"
        );
        return Err(status);
    }
    // 0: the pack or index declined (miss, unsupported, oversized lane, ...).
    Ok(None)
}
