//! Series X SDR 4K profiles for the title's PC FidelityFX dispatch.
use anyhow::{Context, Result, ensure};
use victory_loader::PreparedImage;

const RETAIL_BUILD: &str = "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93";
const SDR_MODE: u32 = 0x0615_c100;
const STOCK_SDR_MODE: [u8; 16] = [0, 15, 0, 0, 112, 8, 0, 0, 60, 0, 0, 0, 1, 0, 0, 0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FsrPreset {
    /// Native resolution through the title's own no-FSR branch: the scene
    /// renders at the output size, and the temporal renderer is left as the
    /// title chose it (not forced to the FSR one).
    Native,
    Quality,
    Balanced,
    Performance,
}

impl FsrPreset {
    pub(super) fn requested() -> Result<Option<Self>> {
        match std::env::var("VICTORY_FSR_4K") {
            Err(std::env::VarError::NotPresent) => Ok(None),
            Ok(name) if name == "0" => Ok(None),
            Ok(name) => Self::from_name(&name.trim().to_ascii_lowercase())
                .map(Some)
                .with_context(|| {
                    format!("VICTORY_FSR_4K={name}: expected native, quality, balanced, performance or 0")
                }),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "native" => Some(Self::Native),
            "quality" => Some(Self::Quality),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            _ => None,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Quality => "quality",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    // AMD SDK FFX_UPSCALE_QUALITY_MODE values and ratios.
    pub(super) fn mode(self) -> u8 {
        match self {
            // Native takes the title's no-FSR branch, which stores a bool.
            Self::Native => 0,
            Self::Quality => 1,
            Self::Balanced => 2,
            Self::Performance => 3,
        }
    }

    pub(super) fn render_scale(self) -> f32 {
        match self {
            Self::Native => 1.0,
            Self::Quality => 2.0 / 3.0,
            Self::Balanced => 1.0 / 1.7,
            Self::Performance => 0.5,
        }
    }

    pub(super) fn render_size(self) -> [u32; 2] {
        [3840.0, 2160.0].map(|size| (size * self.render_scale()) as u32)
    }
}

pub(super) fn prepare_fsr_4k(image: &mut PreparedImage, preset: FsrPreset) -> Result<()> {
    ensure!(
        image.original_hash.to_hex().as_str() == RETAIL_BUILD,
        "FSR at 4K is unavailable for this retail game build"
    );
    ensure!(
        *field::<16>(image, SDR_MODE)? == STOCK_SDR_MODE,
        "Unexpected Series X SDR mode"
    );
    ensure!(
        *field::<4>(image, 0x0615_d95c)? == (2.0f32 / 3.0).to_le_bytes(),
        "Unexpected Series X FSR scene scale"
    );
    ensure!(
        *field::<8>(image, 0x007e_9421)? == [0xb8, 6, 0, 0, 0, 0x0f, 0x45, 0xc1],
        "Unexpected Series X temporal renderer selection"
    );
    ensure!(
        *field::<14>(image, 0x028e_f69a)?
            == [
                0x0f, 0xb6, 0xc0, 0x41, 0x89, 0x86, 0x90, 0, 0, 0, 0x84, 0xc0, 0x75, 0x0a
            ],
        "Unexpected Series X FSR render size selection"
    );
    const NATIVE_LOAD: u32 = 0x028e_f6a8;
    const QUALITY_LOAD: u32 = 0x028e_f6b2;
    // The title's own no-FSR branch loads a 1.0 scene scale; native takes it.
    if preset == FsrPreset::Native {
        ensure!(
            *field::<4>(image, 0x05fc_6ed8)? == 1.0f32.to_le_bytes(),
            "Unexpected Series X native render scale"
        );
    }
    let scaled = matches!(preset, FsrPreset::Balanced | FsrPreset::Performance);
    if scaled {
        ensure!(
            *field::<10>(image, NATIVE_LOAD)?
                == [0xc5, 0xfa, 0x10, 0x05, 0x28, 0x78, 0x6d, 0x03, 0xeb, 0x08]
                && *field::<8>(image, QUALITY_LOAD)?
                    == [0xc5, 0xfa, 0x10, 0x05, 0xa2, 0xe2, 0x86, 0x03],
            "Unexpected Series X FSR scale instructions"
        );
    }
    // An explicit FSR session uses renderer 7 every time it queries this. A
    // native session leaves that choice to the title.
    if preset != FsrPreset::Native {
        *field::<8>(image, 0x007e_9421)? = [0xb8, 7, 0, 0, 0, 0x90, 0x90, 0x90];
    }
    if preset == FsrPreset::Native {
        // Store zero and stay on the no-FSR branch: `xor eax,eax; mov
        // [r14+0x90],eax; test al,al; jne +0x0a (not taken); nop`, the same
        // length as the code replaced.
        *field::<14>(image, 0x028e_f69a)? = [
            0x31, 0xc0, 0x41, 0x89, 0x86, 0x90, 0, 0, 0, 0x84, 0xc0, 0x75, 0x0a, 0x90,
        ];
    } else {
        // Record the SDK mode and jump to the scaled-scene branch.
        *field::<14>(image, 0x028e_f69a)? = [
            0xb8,
            preset.mode(),
            0,
            0,
            0,
            0x41,
            0x89,
            0x86,
            0x90,
            0,
            0,
            0,
            0xeb,
            0x0a,
        ];
    }
    if scaled {
        // The jump makes the native-scale instruction unreachable. Reuse
        // its RIP operand bytes for a private literal, leaving the constants
        // used by other passes unchanged.
        let local_scale = NATIVE_LOAD + 4;
        *field::<4>(image, local_scale)? = preset.render_scale().to_le_bytes();
        let displacement = local_scale as i32 - (QUALITY_LOAD as i32 + 8);
        *field::<4>(image, QUALITY_LOAD + 4)? = displacement.to_le_bytes();
    }
    tracing::info!(
        renderer = (preset != FsrPreset::Native).then_some(7),
        preset = preset.name(),
        quality = preset.mode(),
        expected_render = ?preset.render_size(),
        output = ?[3840, 2160],
        "Selected Series X FSR with the stock 4K frontend"
    );
    Ok(())
}

fn field<const N: usize>(image: &mut PreparedImage, rva: u32) -> Result<&mut [u8; N]> {
    let offset = image
        .info
        .rva_to_file_offset(rva)
        .context("Display profile field missing")? as usize;
    image
        .bytes
        .get_mut(offset..offset + N)
        .context("Incomplete display profile field")?
        .try_into()
        .context("Invalid display profile field length")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsr_presets_leave_the_stock_scanout_and_settings_viewport() {
        let Some(path) = std::env::var_os("VICTORY_DISPLAY_PROFILE_TEST_IMAGE") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        for (preset, size) in [
            (FsrPreset::Native, [3840, 2160]),
            (FsrPreset::Quality, [2560, 1440]),
            (FsrPreset::Balanced, [2258, 1270]),
            (FsrPreset::Performance, [1920, 1080]),
        ] {
            let mut image = victory_loader::prepare(&bytes).unwrap();
            let scanout = *field::<5>(&mut image, 0x0247_ad4a).unwrap();
            let settings = *field::<8>(&mut image, 0x0580_48d4).unwrap();
            prepare_fsr_4k(&mut image, preset).unwrap();
            assert_eq!(*field::<16>(&mut image, SDR_MODE).unwrap(), STOCK_SDR_MODE);
            assert_eq!(*field::<5>(&mut image, 0x0247_ad4a).unwrap(), scanout);
            assert_eq!(*field::<8>(&mut image, 0x0580_48d4).unwrap(), settings);
            if preset == FsrPreset::Native {
                // The no-FSR branch: zero stored, and both the renderer
                // choice and the 1.0 native scale left as the title has them.
                assert_eq!(*field::<2>(&mut image, 0x028e_f69a).unwrap(), [0x31, 0xc0]);
                assert_eq!(*field::<4>(&mut image, 0x007e_9422).unwrap(), 6u32.to_le_bytes());
                let disp = i32::from_le_bytes(*field::<4>(&mut image, 0x028e_f6ac).unwrap());
                let reference = 0x028e_f6b0_u32.wrapping_add_signed(disp);
                let scale = f32::from_le_bytes(*field::<4>(&mut image, reference).unwrap());
                assert_eq!(scale, 1.0);
            } else {
                assert_eq!(
                    *field::<4>(&mut image, 0x007e_9422).unwrap(),
                    7u32.to_le_bytes()
                );
                assert_eq!(
                    *field::<4>(&mut image, 0x028e_f69b).unwrap(),
                    u32::from(preset.mode()).to_le_bytes()
                );
                let disp = i32::from_le_bytes(*field::<4>(&mut image, 0x028e_f6b6).unwrap());
                let reference = 0x028e_f6ba_u32.wrapping_add_signed(disp);
                let scale = f32::from_le_bytes(*field::<4>(&mut image, reference).unwrap());
                assert_eq!([3840.0, 2160.0].map(|w| (w * scale) as u32), size);
            }
            assert_eq!(preset.render_size(), size);
        }
    }
}
