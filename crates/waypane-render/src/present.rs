use wgpu::TextureFormat;

const PASSTHROUGH_SURFACE_FORMATS: [TextureFormat; 2] =
    [TextureFormat::Bgra8Unorm, TextureFormat::Rgba8Unorm];

/// Canvas pixels already carry Wayland's sRGB-encoded color values. Unlike sRGB targets, plain
/// 8-bit UNORM targets store a sampled texel unchanged, so prefer them when the surface offers
/// one; otherwise keep the adapter's preferred format and convert through a matching source.
pub fn preferred_surface_format(supported: &[TextureFormat]) -> Option<TextureFormat> {
    PASSTHROUGH_SURFACE_FORMATS
        .into_iter()
        .find(|format| supported.contains(format))
        .or_else(|| supported.first().copied())
}

/// Selects the upload format for sRGB-encoded canvas or cursor pixels drawn into `target`.
/// Writing to an sRGB target encodes linear values, so the source must decode on sampling
/// (`Rgba8UnormSrgb`) for the two to cancel out. Float and wide-range targets expect linear
/// values as well. Plain UNORM targets store encoded values directly (`Rgba8Unorm`).
pub fn canvas_texture_format(target: TextureFormat) -> TextureFormat {
    if PASSTHROUGH_SURFACE_FORMATS.contains(&target) {
        TextureFormat::Rgba8Unorm
    } else {
        TextureFormat::Rgba8UnormSrgb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_encoded_passthrough_surfaces_over_adapter_order() {
        let supported = [
            TextureFormat::Bgra8UnormSrgb,
            TextureFormat::Bgra8Unorm,
            TextureFormat::Rgba16Float,
        ];
        let surface = preferred_surface_format(&supported);
        assert_eq!(surface, Some(TextureFormat::Bgra8Unorm));
        assert_eq!(
            canvas_texture_format(surface.unwrap()),
            TextureFormat::Rgba8Unorm
        );
    }

    #[test]
    fn falls_back_to_adapter_choice_and_matches_its_transfer_function() {
        assert_eq!(
            preferred_surface_format(&[TextureFormat::Rgba8Unorm]),
            Some(TextureFormat::Rgba8Unorm)
        );
        for target in [
            TextureFormat::Bgra8UnormSrgb,
            TextureFormat::Rgba8UnormSrgb,
            TextureFormat::Rgba16Float,
        ] {
            assert_eq!(
                preferred_surface_format(&[target]),
                Some(target),
                "{target:?}"
            );
            assert_eq!(
                canvas_texture_format(target),
                TextureFormat::Rgba8UnormSrgb,
                "{target:?}"
            );
        }
    }

    #[test]
    fn accepts_empty_and_hdr_only_surfaces() {
        assert_eq!(preferred_surface_format(&[]), None);
        let hdr = [TextureFormat::Rgba16Float, TextureFormat::Rgb10a2Unorm];
        assert_eq!(preferred_surface_format(&hdr), hdr.first().copied());
    }
}
