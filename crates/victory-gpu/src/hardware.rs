//! What the PC's memory is like under the card, and the strategy it picks.
//!
//! The console's memory is one pool the CPU and the GPU both reach, and a
//! title writes a texture where its shaders read it. A PC splits it: the
//! card's memory (L1) is behind the bus, the PC's (L0) is not, and what the
//! layer may do about that depends on the hardware, which the host device
//! answers itself:
//!
//! - `D3D12_FEATURE_ARCHITECTURE` says whether the card shares the PC's
//!   memory (`UMA`) and whether the CPU's caches and the card's agree
//!   (`CacheCoherentUMA`). An APU, a handheld or a console-like SoC says
//!   yes to both: a buffer made in the PC's memory, mapped, is the card's
//!   memory too, and nothing has to be copied to it.
//! - `D3D12_FEATURE_DATA_D3D12_OPTIONS16.GPUUploadHeapSupported` says
//!   whether the card's own memory can be mapped by the CPU (a resizable
//!   BAR, and a driver that offers the heap type): the CPU writes into the
//!   card's memory across the bus, and no copy stands between a title's
//!   write and a shader's read.
//!
//! With neither, the CPU writes upload memory in the PC's memory (write-
//! combined: sequential writes, no reads) and the queue copies it up.
//!
//! | Tier | What the device says | What memory is | The layer's heap |
//! |---|---|---|---|
//! | [`Tier::Unified`] | UMA and cache-coherent | one pool | custom, write-back, L0 |
//! | [`Tier::MappedCard`] | `GPUUploadHeapSupported` | split, the card's mappable | `GPU_UPLOAD` |
//! | [`Tier::Discrete`] | neither | split | `UPLOAD`, then copies |
//!
//! `VICTORY_MEMORY_TIER=unified|mapped|discrete` overrules the query, for an
//! A/B between two runs on one machine. The query is made once, at the host
//! device's creation ([`install`]); what the title is told about the
//! console's memory is not this (it is always told the console's one pool,
//! `device::check_feature_support`).

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::*;

/// The most the layer can do with the PC's memory under this card, best
/// first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    /// One pool of memory for the CPU and the card (UMA, cache-coherent).
    Unified,
    /// The card's memory, which the CPU maps (a resizable BAR).
    MappedCard,
    /// The PC's memory for the CPU, the card's for shaders, a copy between.
    Discrete,
}

impl Tier {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Tier::Unified => "unified",
            Tier::MappedCard => "mapped card memory (BAR)",
            Tier::Discrete => "discrete (upload, then copy)",
        }
    }
}

/// What the host device answered.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Answers {
    /// `D3D12_FEATURE_DATA_ARCHITECTURE.UMA`.
    pub(crate) uma: bool,
    /// `D3D12_FEATURE_DATA_ARCHITECTURE.CacheCoherentUMA`.
    pub(crate) coherent: bool,
    /// `D3D12_FEATURE_DATA_D3D12_OPTIONS16.GPUUploadHeapSupported`.
    pub(crate) gpu_upload: bool,
}

/// The tier the answers ask for, before [`overruled`].
fn choose(answers: Answers) -> Tier {
    if answers.uma && answers.coherent {
        Tier::Unified
    } else if answers.gpu_upload {
        Tier::MappedCard
    } else {
        Tier::Discrete
    }
}

/// The tier `VICTORY_MEMORY_TIER` names, if it names one.
fn overruled(value: Option<&str>) -> Option<Tier> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("unified") | Some("uma") => Some(Tier::Unified),
        Some("mapped") | Some("bar") | Some("rebar") => Some(Tier::MappedCard),
        Some("discrete") => Some(Tier::Discrete),
        _ => None,
    }
}

/// What the device creation found: its answers, and the tier in use.
#[derive(Clone, Copy)]
struct Chosen {
    answers: Answers,
    tier: Tier,
}

static CHOSEN: OnceLock<Chosen> = OnceLock::new();

/// Asks `device` about the PC's memory; logs the answers and the tier, and
/// keeps them for [`tier`] and [`maps_card_memory`]. Called once, when the
/// host device is made.
pub(crate) fn install(device: &ID3D12Device) -> Tier {
    let answers = probe(device);
    let asked = choose(answers);
    let tier = overruled(std::env::var("VICTORY_MEMORY_TIER").ok().as_deref()).unwrap_or(asked);
    if tier != asked {
        tracing::info!(
            asked = asked.name(),
            tier = tier.name(),
            "the memory tier is overruled (VICTORY_MEMORY_TIER)"
        );
    }
    // A tier asked for that the card cannot do falls back where it must:
    // saying `mapped` on a card without `GPU_UPLOAD` would have the layer
    // map memory it cannot.
    if matches!(tier, Tier::MappedCard) && !answers.gpu_upload {
        tracing::warn!(
            answers = ?answers,
            "the memory tier asked for is not there: the card has no GPU upload heap; using the discrete path"
        );
    }
    tracing::info!(
        uma = answers.uma,
        cache_coherent = answers.coherent,
        gpu_upload = answers.gpu_upload,
        tier = tier.name(),
        "the PC's memory under the card"
    );
    let _ = CHOSEN.set(Chosen { answers, tier });
    tier
}

/// Asks the device the two feature queries [`install`] keeps.
pub(crate) fn probe(device: &ID3D12Device) -> Answers {
    let mut architecture = D3D12_FEATURE_DATA_ARCHITECTURE::default();
    // SAFETY: the query writes the structure of the size given.
    let architecture_ok = unsafe {
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_ARCHITECTURE,
                (&raw mut architecture).cast(),
                size_of_val(&architecture) as u32,
            )
            .is_ok()
    };
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
    // SAFETY: as above.
    let options_ok = unsafe {
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS16,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .is_ok()
    };
    if !architecture_ok || !options_ok {
        tracing::debug!(
            architecture_ok,
            options_ok,
            "the PC's memory queries are not all there"
        );
    }
    Answers {
        uma: architecture_ok && architecture.UMA.as_bool(),
        coherent: architecture_ok && architecture.CacheCoherentUMA.as_bool(),
        gpu_upload: options_ok && options.GPUUploadHeapSupported.as_bool(),
    }
}

/// The tier in use (the query's, or `VICTORY_MEMORY_TIER`'s). Before the
/// device is made: the discrete path, which asks nothing of the card.
pub(crate) fn tier() -> Tier {
    CHOSEN.get().map_or(Tier::Discrete, |c| c.tier)
}

/// Whether the CPU can map memory that the card's shaders read without a
/// copy: the console's one pool on an APU, the card's own memory behind a
/// resizable BAR, nothing on a discrete card.
pub(crate) fn maps_card_memory() -> bool {
    match tier() {
        Tier::Unified => true,
        Tier::MappedCard => CHOSEN.get().is_some_and(|c| c.answers.gpu_upload),
        Tier::Discrete => false,
    }
}

/// The heap a buffer the CPU writes and the card's shaders read is made in,
/// as the properties a resource is created with: the custom page property
/// and pool for [`Tier::Unified`] (the one pool), the standard type else
/// ([`Tier::MappedCard`]'s `GPU_UPLOAD`, [`Tier::Discrete`]'s `UPLOAD`).
/// Asked only where [`maps_card_memory`] said there is such memory.
pub(crate) fn mapped_properties() -> D3D12_HEAP_PROPERTIES {
    match tier() {
        // The one pool: write-back pages in L0, which the card reaches at
        // the PC's speed.
        Tier::Unified => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        },
        Tier::MappedCard => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_GPU_UPLOAD,
            ..Default::default()
        },
        Tier::Discrete => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(uma: bool, coherent: bool, gpu_upload: bool) -> Answers {
        Answers {
            uma,
            coherent,
            gpu_upload,
        }
    }

    #[test]
    fn one_pool_is_unified_only_when_the_caches_agree() {
        // The console's shape.
        assert_eq!(choose(answers(true, true, false)), Tier::Unified);
        assert_eq!(choose(answers(true, true, true)), Tier::Unified);
        // UMA without coherence is not the console's: a copy is still owed.
        assert_eq!(choose(answers(true, false, true)), Tier::MappedCard);
        assert_eq!(choose(answers(true, false, false)), Tier::Discrete);
        // The card's memory, mappable.
        assert_eq!(choose(answers(false, false, true)), Tier::MappedCard);
        // Neither.
        assert_eq!(choose(answers(false, false, false)), Tier::Discrete);
    }

    #[test]
    fn the_switch_names_the_three_tiers() {
        assert_eq!(overruled(Some("unified")), Some(Tier::Unified));
        assert_eq!(overruled(Some(" UMA ")), Some(Tier::Unified));
        assert_eq!(overruled(Some("Mapped")), Some(Tier::MappedCard));
        assert_eq!(overruled(Some("rebar")), Some(Tier::MappedCard));
        assert_eq!(overruled(Some("discrete")), Some(Tier::Discrete));
        // Not a tier: the query's answer stands.
        assert_eq!(overruled(Some("apu")), None);
        assert_eq!(overruled(Some("")), None);
        assert_eq!(overruled(None), None);
    }

    #[test]
    fn before_the_device_is_made_nothing_is_asked_of_the_card() {
        // (Another test may have installed a device already; this only
        // checks that the accessors answer without one.)
        let tier = tier();
        assert!(matches!(
            tier,
            Tier::Unified | Tier::MappedCard | Tier::Discrete
        ));
        if tier == Tier::Discrete {
            assert!(!maps_card_memory());
        }
    }

    /// The two queries, as the software adapter answers them: the same calls
    /// the card gets, so a wrong structure or feature number shows here.
    #[test]
    fn the_probe_asks_the_device_and_the_software_adapter_answers() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = crate::inplace::tests::warp() else {
            return;
        };
        let mut architecture = D3D12_FEATURE_DATA_ARCHITECTURE::default();
        // SAFETY: the query writes the structure of the size given.
        let architecture_ok = unsafe {
            device
                .CheckFeatureSupport(
                    D3D12_FEATURE_ARCHITECTURE,
                    (&raw mut architecture).cast(),
                    size_of_val(&architecture) as u32,
                )
                .is_ok()
        };
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
        // SAFETY: as above.
        let options_ok = unsafe {
            device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS16,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
                .is_ok()
        };
        assert!(architecture_ok, "the architecture query is not understood");
        // (A runtime that does not know OPTIONS16 answers E_INVALIDARG:
        // then no GPU upload heap, which is what the probe then says.)
        let answers = probe(&device);
        assert_eq!(answers.uma, architecture.UMA.as_bool());
        assert_eq!(answers.coherent, architecture.CacheCoherentUMA.as_bool());
        assert_eq!(
            answers.gpu_upload,
            options_ok && options.GPUUploadHeapSupported.as_bool()
        );
        assert_eq!(
            choose(answers),
            if answers.uma && answers.coherent {
                Tier::Unified
            } else if answers.gpu_upload {
                Tier::MappedCard
            } else {
                Tier::Discrete
            }
        );
    }
}
