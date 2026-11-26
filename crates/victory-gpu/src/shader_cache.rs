//! Translated shaders kept on disk (decision 0003): under the configured
//! cache folder, keyed by the container's content, by the translator's
//! version ([`victory_shader::TRANSLATOR_VERSION`]) and by its settings.
//! A rebuild that leaves the version keeps every translation: the store
//! grows by the shaders a change made translate, not by a whole new set
//! each build (16 GB of sets of earlier builds had piled up when the key
//! was the translator's source). Entries go through `victory-pipeline`'s
//! content-addressed cache, which writes them atomically; an entry that
//! does not read back is translated again.
//!
//! Each entry also names the build of the translator that made it (the
//! BLAKE3 of its sources, `build.rs`). One of an earlier build is served
//! as it is, and counted; `victory shaders` ([`precompile`]) translates it
//! again, keeps the new one and says how many came out different (then
//! the version should have been bumped). `VICTORY_SHADER_FRESH=1` does the
//! same in a run; `VICTORY_SHADER_KEY=source` keys by the translator's
//! source as before (a separate set for each build).
//!
//! The entry is our own encoding of [`HostShader`]: a field added to it
//! must be added to `encode` and `decode` too, and [`FORMAT`] bumped.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The encoding of entries ([`encode`]), part of the key.
const FORMAT: u32 = 2;

use victory_pipeline::{Cache, Converter};
use victory_shader::emit::{HostShader, SpecConstant, SpecValue, StaticSampler, UserData};
use victory_shader::resource::RootIndexedView;
use victory_shader::{HwStage, ResourceType, RootType, UsedResource};

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
/// Hits on entries an earlier build of the translator made (served as they
/// are, or when refreshing translated again); of those translated again,
/// how many came out different.
static STALE: AtomicU64 = AtomicU64::new(0);
static REFRESHED: AtomicU64 = AtomicU64::new(0);
static CHANGED: AtomicU64 = AtomicU64::new(0);

/// The build of the translator (the BLAKE3 of its sources, `build.rs`).
const SOURCE: &str = env!("VICTORY_TRANSLATOR");

/// How cached translations are keyed (`VICTORY_SHADER_KEY`).
fn keyed_by_source() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_SHADER_KEY").is_ok_and(|v| v == "source"))
}

/// Whether a run translates entries of an earlier build again
/// (`VICTORY_SHADER_FRESH=1`; [`precompile`] always does).
fn refresh_in_runs() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SHADER_FRESH").is_some_and(|v| v == "1"))
}

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// Since the start: entries of an earlier build of the translator met,
/// those of them translated again, and those that came out different.
pub fn stale_counts() -> (u64, u64, u64) {
    (STALE.load(Ordering::Relaxed), REFRESHED.load(Ordering::Relaxed), CHANGED.load(Ordering::Relaxed))
}

/// What identifies a translation beside its container: the translator's
/// version (or with `VICTORY_SHADER_KEY=source` its sources), the form of
/// entries, the settings it runs with, and what it was told of the host.
/// Not our executable: a rebuild of anything else used to translate every
/// shader again.
fn build() -> [u8; 32] {
    // The view heap's null slot differs by host GPU: a translation cached
    // without the clamp must not be served (`host_settings` below).
    crate::descriptor::host::probe_view_limit();
    static FIXED: OnceLock<Vec<u8>> = OnceLock::new();
    let fixed = FIXED.get_or_init(|| {
        let limit = victory_shader::emit::loop_limit();
        if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
            tracing::warn!(
                limit,
                "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"
            );
        }
        let mut bytes = if keyed_by_source() {
            SOURCE.as_bytes().to_vec()
        } else {
            format!("version {}", victory_shader::TRANSLATOR_VERSION).into_bytes()
        };
        bytes.extend_from_slice(&FORMAT.to_le_bytes());
        bytes.extend_from_slice(&limit.to_le_bytes());
        // Which group barriers wait is part of the translation too.
        bytes.push(victory_shader::emit::group_sync());
        // So are the switches that pick the form of a wave64 program:
        // without them here a run with `VICTORY_WAVE_WIDE=0` was
        // served the translations an earlier run of the same
        // executable had cached with the switch unset.
        for name in [
            "VICTORY_WAVE_WIDE",
            "VICTORY_WAVE_DEEP_SKIPS",
            "VICTORY_WAVE_OWN_BITS",
            "VICTORY_WAVE_LOCAL",
            "VICTORY_LDS_FLAGS",
            "VICTORY_F16_NEAREST",
            "VICTORY_HELPER_OWN",
            "VICTORY_FIRSTLANE_OWN",
            "VICTORY_FIRST_HALF",
            "VICTORY_STRICT_JOINS",
            "VICTORY_MESH_OUTPUT_STRICT",
            "VICTORY_BVH_FENCES",
            "VICTORY_BVH_BOUNDS",
        ] {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(b'=');
            if let Some(value) = std::env::var_os(name) {
                bytes.extend_from_slice(value.as_encoded_bytes());
            }
            bytes.push(0);
        }
        // And which clean-up passes ran over the translated code.
        bytes.extend_from_slice(&victory_shader::emit::shader_opt().to_le_bytes());
        // Renderer port 5 (`VICTORY_PORT_CBV=1`): constant buffers read as raw
        // bytes. Only when on, so the translations cached without it keep
        // their key.
        if victory_shader::emit::constants_raw() {
            bytes.extend_from_slice(b"VICTORY_PORT_CBV=1");
        }
        bytes
    });
    let mut hash = blake3::Hasher::new();
    hash.update(fixed);
    for value in victory_shader::emit::host_settings() {
        hash.update(&value.to_le_bytes());
    }
    *hash.finalize().as_bytes()
}

/// What tells one set of translations from another ([`build`]).
pub(crate) fn translation() -> [u8; 32] {
    build()
}

/// Hex-encoded fingerprint of the active shader translation settings.
pub fn translation_hex() -> String {
    blake3::Hash::from_bytes(translation()).to_hex().to_string()
}

/// Puts the translation of `container` into the cache before any title asks
/// for it (`victory shaders`): whether it was there already, or why it does
/// not translate. The host is opened first, as a run does: what the card
/// can do decides the form of some programs.
pub fn precompile(container: &[u8]) -> Result<bool, String> {
    let _ = crate::host::host();
    let misses = MISSES.load(Ordering::Relaxed);
    let translate = |c: &[u8]| victory_shader::emit::translate(c).map_err(|e| e.to_string());
    match CACHE.get() {
        Some(cache) => {
            let _ = cached(cache, &build(), container, translate, true)?;
        }
        None => {
            translate(container)?;
        }
    };
    // (Counts other threads' misses too: the caller only adds them up.)
    Ok(MISSES.load(Ordering::Relaxed) == misses)
}

/// The translation of a console shader container, from the cache when
/// there is one.
/// The translation of a console shader container, from the cache when there
/// is one, with the mnemonics of `disabled` lowered as no-ops
/// (the driver-crash bisect's fallback). The disabled set is part of the
/// translation fingerprint, so a fallback translation is cached apart from
/// the normal one.
pub(crate) fn host_shader_with(container: &[u8], disabled: &[String]) -> Result<(HostShader, Meta), String> {
    let translate = |c: &[u8]| victory_shader::emit::translate_with(c, disabled).map_err(|e| e.to_string());
    match CACHE.get() {
        Some(cache) => cached(cache, &build_with(disabled), container, translate, refresh_in_runs()),
        None => {
            let shader = translate(container)?;
            Ok((shader, meta_of(container)))
        }
    }
}

/// [`build`] with the disabled mnemonics folded in, so a fallback
/// translation is cached apart from the normal one.
fn build_with(disabled: &[String]) -> [u8; 32] {
    if disabled.is_empty() {
        return build();
    }
    let mut h = blake3::Hasher::new();
    h.update(&build());
    for m in disabled {
        h.update(&(m.len() as u64).to_le_bytes());
        h.update(m.as_bytes());
    }
    *h.finalize().as_bytes()
}

/// The cache entry's converter: the build and the container in, the
/// encoded translation out. `ran` tells a miss from a hit.
struct Translate<'a, F> {
    translate: F,
    /// The build of the translator, written into the entry.
    source: &'a str,
    ran: AtomicBool,
}

impl<F: Fn(&[u8]) -> Result<HostShader, String> + Send + Sync> Converter for Translate<'_, F> {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        // The build fingerprint in the input does the versioning.
        1
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let container = &input[32..];
        let shader = (self.translate)(container).map_err(anyhow::Error::msg)?;
        let meta = meta_of(container);
        Ok(encode(&shader, Some(&meta), self.source))
    }
}

/// `refresh`: an entry an earlier build of the translator made is
/// translated again (and replaced) rather than served.
fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
    refresh: bool,
) -> Result<(HostShader, Meta), String> {
    cached_by(cache, build, SOURCE, container, translate, refresh)
}

/// [`cached`] with the build of the translator given (`source`).
fn cached_by(
    cache: &Cache,
    build: &[u8; 32],
    source: &str,
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
    refresh: bool,
) -> Result<(HostShader, Meta), String> {
    let converter = Translate {
        translate,
        source,
        ran: AtomicBool::new(false),
    };
    let mut old: Option<HostShader> = None;
    for _ in 0..3 {
        let (path, bytes) = cache
            .read_or_convert_parts(&converter, &[&build[..], container])
            .map_err(|e| format!("{e:#}"))?;
        if let Some((shader, meta, made_by)) = decode(&bytes) {
            let ran = converter.ran.load(Ordering::Relaxed);
            if !ran && made_by != source.as_bytes() {
                STALE.fetch_add(1, Ordering::Relaxed);
                if refresh && old.is_none() {
                    // Translated again in its place; what came out before
                    // is what the new one is compared with.
                    tracing::debug!(path = %path.display(), "shader cache entry of an earlier translator; translating again");
                    old = Some(shader);
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                static SAID: AtomicBool = AtomicBool::new(false);
                if !SAID.swap(true, Ordering::Relaxed) {
                    tracing::info!("shader translations of an earlier build of the translator (same version) are served; `victory shaders` translates them again");
                }
            }
            if let Some(old) = old.take() {
                REFRESHED.fetch_add(1, Ordering::Relaxed);
                if old != shader {
                    CHANGED.fetch_add(1, Ordering::Relaxed);
                }
            }
            // A VSH2 entry has no metadata: compute it once (single decode)
            // and upgrade the entry in place, without re-translating.
            let meta = match meta {
                Some(meta) => meta,
                None => {
                    let meta = meta_of(container);
                    let _ = std::fs::write(&path, encode(&shader, Some(&meta), source));
                    meta
                }
            };
            let (counter, what) = if ran {
                (&MISSES, "translated and cached")
            } else {
                (&HITS, "from the cache")
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "shader {what}");
            return Ok((shader, meta));
        }
        tracing::debug!(path = %path.display(), "unreadable shader cache entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    let shader = (converter.translate)(container)?;
    Ok((shader, meta_of(container)))
}

const MAGIC: &[u8; 4] = b"VSH3";
/// The encoding before [`Meta`] was in the entry: decoded as it is (no
/// metadata), then upgraded in place after one re-analysis.
const MAGIC_V2: &[u8; 4] = b"VSH2";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

fn used_code(kind: ResourceType) -> u32 {
    match kind {
        ResourceType::Srv => 0,
        ResourceType::Sampler => 1,
        ResourceType::Cbv => 2,
        ResourceType::Uav => 3,
        ResourceType::Other(x) => 4 + u32::from(x),
    }
}

fn used_kind(code: u32) -> ResourceType {
    match code {
        0 => ResourceType::Srv,
        1 => ResourceType::Sampler,
        2 => ResourceType::Cbv,
        3 => ResourceType::Uav,
        x => ResourceType::Other((x - 4) as u8),
    }
}

/// What a VSH3 entry carries beside the [`HostShader`], so a hit skips
/// parsing the container and decoding its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) threads: Option<[u32; 3]>,
    pub(crate) used: Vec<UsedResource>,
    pub(crate) stores: bool,
    pub(crate) stores_by_address: bool,
}

/// The [`Meta`] of `container`, from its native programs (one decode for both
/// store kinds, via [`victory_shader::analyze_stores`]).
fn meta_of(container: &[u8]) -> Meta {
    use victory_shader::{Container, FourCC, HwStage};
    let Ok(c) = Container::parse(container) else {
        return Meta { threads: None, used: Vec::new(), stores: true, stores_by_address: true };
    };
    let programs = match c.part(FourCC::DXIL).or(c.part(FourCC::ACCC)) {
        Some(part) => victory_shader::native_programs(part.data).unwrap_or_default(),
        None => Vec::new(),
    };
    let threads = programs.iter().find_map(|p| match &p.io {
        victory_shader::native::StageIo::Cs(cs) if cs.threads.iter().all(|&n| n > 0) => Some(cs.threads),
        _ => None,
    });
    let (mut stores, mut stores_by_address) = (programs.is_empty(), programs.is_empty());
    for p in &programs {
        let (s, a) = victory_shader::analyze_stores(p);
        stores |= s;
        stores_by_address |= a;
    }
    let used = programs
        .into_iter()
        .find(|p| matches!(p.stage, HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs))
        .map(|p| p.used)
        .unwrap_or_default();
    Meta { threads, used, stores, stores_by_address }
}

/// Little-endian words and length-prefixed lists, after the build of the
/// translator that made the entry (`source`, length-prefixed).
fn encode(s: &HostShader, meta: Option<&Meta>, source: &str) -> Vec<u8> {
    let mut out = match meta {
        Some(_) => MAGIC.to_vec(),
        None => MAGIC_V2.to_vec(),
    };
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    word(&mut out, source.len() as u32);
    out.extend(source.as_bytes());
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs, &s.root_uavs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    word(&mut out, s.root_indexed_views.len() as u32);
    for view in &s.root_indexed_views {
        for value in [
            u32::from(view.table_slot),
            u32::from(view.constant_slot),
            view.constant_dword,
            view.byte_offset,
            u32::from(view.shift),
        ] {
            word(&mut out, value);
        }
    }
    word(&mut out, s.spec_constants.len() as u32);
    for spec in &s.spec_constants {
        let (tag, bits) = match spec.default {
            SpecValue::U32(v) => (0, v),
            SpecValue::I32(v) => (1, v as u32),
            SpecValue::F32(v) => (2, v.to_bits()),
        };
        for value in [spec.id, tag, bits, spec.offset_bits] {
            word(&mut out, value);
        }
    }
    if let Some(meta) = meta {
        // Meta (VSH3): one decode's worth of the container's programs.
        match meta.threads {
            Some([x, y, z]) => {
                word(&mut out, 1);
                for v in [x, y, z] {
                    word(&mut out, v);
                }
            }
            None => word(&mut out, 0),
        }
        word(&mut out, u32::from(meta.stores));
        word(&mut out, u32::from(meta.stores_by_address));
        word(&mut out, meta.used.len() as u32);
        for u in &meta.used {
            for v in [
                used_code(u.kind),
                root_code(u.root),
                u.table_offset_dwords,
                u.lower_bound,
                u.stride,
                u32::from(u.dwords),
                u32::from(u.user_data),
                u32::from(u.slot),
            ] {
                word(&mut out, v);
            }
        }
    }
    out
}

/// A cursor over an entry's bytes.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }
    fn word(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn list(&mut self) -> Option<Vec<u8>> {
        let n = self.word()? as usize;
        Some(self.take(n)?.to_vec())
    }
}

/// The shader, its [`Meta`] (VSH3 only; `None` for a VSH2 entry), and the
/// build of the translator that made it.
fn decode(bytes: &[u8]) -> Option<(HostShader, Option<Meta>, Vec<u8>)> {
    let mut r = Reader(bytes);
    let magic = r.take(4)?;
    let v3 = magic == MAGIC;
    if !v3 && magic != MAGIC_V2 {
        return None;
    }
    let made_by = r.list()?;
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let root_uavs = r.list()?;
    let n = r.word()? as usize;
    let mut root_indexed_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        root_indexed_views.push(RootIndexedView {
            table_slot: u8::try_from(r.word()?).ok()?,
            constant_slot: u8::try_from(r.word()?).ok()?,
            constant_dword: r.word()?,
            byte_offset: r.word()?,
            shift: u8::try_from(r.word()?).ok().filter(|&s| s < 32)?,
        });
    }
    let n = r.word()? as usize;
    let mut spec_constants = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        let id = r.word()?;
        let default = match r.word()? {
            0 => SpecValue::U32(r.word()?),
            1 => SpecValue::I32(r.word()? as i32),
            2 => SpecValue::F32(f32::from_bits(r.word()?)),
            _ => return None,
        };
        spec_constants.push(SpecConstant {
            id,
            default,
            offset_bits: r.word()?,
        });
    }
    let meta = if v3 { Some(decode_meta(&mut r)?) } else { None };
    r.0.is_empty().then_some((
        HostShader {
            stage,
            dxil,
            user_data,
            samplers,
            static_samplers,
            root_cbvs,
            root_srvs,
            root_uavs,
            root_indexed_views,
            spec_constants,
        },
        meta,
        made_by,
    ))
}

fn decode_meta(r: &mut Reader) -> Option<Meta> {
    let threads = match r.word()? {
        1 => Some([r.word()?, r.word()?, r.word()?]),
        0 => None,
        _ => return None,
    };
    let stores = r.word()? != 0;
    let stores_by_address = r.word()? != 0;
    let n = r.word()? as usize;
    let mut used = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        used.push(UsedResource {
            kind: used_kind(r.word()?),
            root: root_type(r.word()?),
            table_offset_dwords: r.word()?,
            lower_bound: r.word()?,
            stride: r.word()?,
            dwords: u8::try_from(r.word()?).ok()?,
            user_data: u8::try_from(r.word()?).ok()?,
            slot: u8::try_from(r.word()?).ok()?,
        });
    }
    Some(Meta { threads, used, stores, stores_by_address })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            root_uavs: vec![1],
            root_indexed_views: vec![RootIndexedView {
                table_slot: 1,
                constant_slot: 2,
                constant_dword: 3,
                byte_offset: 32,
                shift: 5,
            }],
            spec_constants: Vec::new(),
        }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate, false).map(|(s, _)| s), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate, false).map(|(s, _)| s), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate, false).map(|(s, _)| s), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH2 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate, false).map(|(s, _)| s), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate, false).map(|(s, _)| s), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Failures are not stored.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        assert!(cached(&cache, &build, b"other", fail, false).is_err());
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    fn meta() -> Meta {
        Meta { threads: None, used: Vec::new(), stores: false, stores_by_address: false }
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        // A VSH3 entry (with its metadata).
        let bytes = encode(&shader(), Some(&meta()), "made by");
        assert_eq!(decode(&bytes), Some((shader(), Some(meta()), b"made by".to_vec())));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);
        // A VSH2 entry (no metadata) decodes as such, so it can be upgraded.
        let v2 = encode(&shader(), None, "made by");
        assert_eq!(decode(&v2), Some((shader(), None, b"made by".to_vec())));
        // Entries of the encoding before the translator's build was in them.
        let mut v1 = bytes.clone();
        v1[3] = b'1';
        assert_eq!(decode(&v1), None);
    }

    /// An entry an earlier build of the translator made, under the same
    /// version: served as it is; when refreshing translated again in its
    /// place, and a different result counted.
    #[test]
    fn entries_of_an_earlier_translator_are_served_or_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [9u8; 32];
        let container = b"a container of an earlier build".as_slice();
        let runs = AtomicU64::new(0);
        let old = || {
            let mut s = shader();
            s.dxil = b"the old translation".to_vec();
            s
        };
        let first = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(old())
        };
        assert_eq!(cached_by(&cache, &build, "build a", container, first, false).map(|(s, _)| s), Ok(old()));
        let now = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        let (stale, refreshed, changed) = stale_counts();
        // The next build, same version: the old translation, not translated.
        assert_eq!(cached_by(&cache, &build, "build b", container, now, false).map(|(s, _)| s), Ok(old()));
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        // Refreshing: translated again and kept.
        assert_eq!(cached_by(&cache, &build, "build b", container, now, true).map(|(s, _)| s), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        // Now the entry is this build's: served when refreshing too.
        assert_eq!(cached_by(&cache, &build, "build b", container, now, true).map(|(s, _)| s), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        // (Other tests count too; these at least.)
        let (s, r, c) = stale_counts();
        assert!(s >= stale + 2 && r > refreshed && c > changed, "{s} {r} {c}");
    }
}
