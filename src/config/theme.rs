//! Theme seed (§6). Pure data + pure functions — unit-testable with zero I/O
//! (I-09). The phase-1 ambient wallpaper is a four-corner gradient whose
//! corner colors drift through the palette on a slow cycle.

/// sRGB color with 8-bit channels. `from_hex` fails loudly on malformed input
/// (the config system in phase 5 keeps the previous config on such errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn from_rgb8(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Parse `#RGB`, `#RRGGBB` or `#RRGGBBAA` (the `#` is optional).
    ///
    /// Unused by the phase-1 wallpaper itself; it is the seed of the phase-5
    /// config parser (§6) and is pinned by unit tests now so it cannot rot.
    #[allow(dead_code)]
    pub fn from_hex(s: &str) -> Result<Self, String> {
        let s = s.strip_prefix('#').unwrap_or(s);
        let expected = match s.len() {
            3 | 4 | 6 | 8 => s.len(),
            _ => {
                return Err(format!(
                    "invalid hex color `{s}`: expected #RGB, #RRGGBB or #RRGGBBAA"
                ));
            }
        };
        let digit = |pair: &str| -> Result<u8, String> {
            u8::from_str_radix(pair, 16).map_err(|_| format!("invalid hex digit in `{pair}`"))
        };
        let (r, g, b, a) = if expected == 3 || expected == 4 {
            let nx = |i: usize| digit(&s[i..i + 1]).map(|d| d * 17);
            (
                nx(0)?,
                nx(1)?,
                nx(2)?,
                if expected == 4 { nx(3)? } else { 255 },
            )
        } else {
            let np = |i: usize| digit(&s[i..i + 2]);
            (
                np(0)?,
                np(2)?,
                np(4)?,
                if expected == 8 { np(6)? } else { 255 },
            )
        };
        Ok(Self { r, g, b, a })
    }

    /// Channels as f32 in [0, 1], still in sRGB space (matches
    /// `*UnormSrgb` surface formats).
    pub fn to_rgba_srgb(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }

    /// Channels as linear-light f32 (use when the target format is NOT an
    /// sRGB format, so the shader output is gamma-encoded by us instead).
    pub fn to_rgba_linear(self) -> [f32; 4] {
        let srgb = self.to_rgba_srgb();
        [
            srgb_to_linear(srgb[0]),
            srgb_to_linear(srgb[1]),
            srgb_to_linear(srgb[2]),
            srgb[3],
        ]
    }

    /// Component-wise linear interpolation in u8 space.
    pub fn lerp(a: Self, b: Self, t: f64) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
        Self {
            r: mix(a.r, b.r),
            g: mix(a.g, b.g),
            b: mix(a.b, b.b),
            a: mix(a.a, b.a),
        }
    }
}

/// Exact sRGB transfer function (IEC 61966-2-1).
pub fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// The theme palette (§6 `[theme]`). Phase 1 consumes
/// `background`/`panel`/`accent`; the remaining fields exist so the phase-5
/// config maps 1:1 onto this struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub background: Color,
    pub panel: Color,
    pub accent: Color,
    pub text: Color,
    pub text_dim: Color,
    pub hit_300: Color,
    pub hit_100: Color,
    pub hit_50: Color,
    pub hit_miss: Color,
    pub slider_track: Color,
    pub slider_fill: Color,
}

/// The default preset (`[theme] preset = "midnight"`, §6).
pub const fn midnight() -> Palette {
    Palette {
        background: Color::from_rgb8(0x0a, 0x0a, 0x14),
        panel: Color::from_rgb8(0x1a, 0x1a, 0x2e),
        accent: Color::from_rgb8(0xff, 0x66, 0xaa),
        text: Color::from_rgb8(0xff, 0xff, 0xff),
        text_dim: Color::from_rgb8(0x88, 0x88, 0xaa),
        hit_300: Color::from_rgb8(0x66, 0xee, 0xff),
        hit_100: Color::from_rgb8(0x88, 0xff, 0x66),
        hit_50: Color::from_rgb8(0xff, 0xcc, 0x44),
        hit_miss: Color::from_rgb8(0xff, 0x44, 0x66),
        slider_track: Color::from_rgb8(0xff, 0x66, 0xaa),
        slider_fill: Color::from_rgb8(0xff, 0xff, 0xff),
    }
}

/// Length of one full corner-color cycle of the ambient gradient, in ms.
pub const GRADIENT_PERIOD_MS: f64 = 8000.0;

/// The ambient wallpaper gradient: four corner colors as a pure function of
/// time. Order is `[top-left, top-right, bottom-left, bottom-right]`.
///
/// The animation lives here — in Rust, unit-tested — while the shader only
/// does spatial interpolation (I-09). Note this is *decorative* time, not
/// gameplay time (I-01: gameplay time arrives with the audio clock in
/// phase 2, §3.3).
pub fn gradient_corners(pal: &Palette, t_ms: f64) -> [Color; 4] {
    let phase = (t_ms / GRADIENT_PERIOD_MS).rem_euclid(1.0);
    let s = 0.5 - 0.5 * (std::f64::consts::TAU * phase).cos();
    let accent_deep = Color::lerp(pal.panel, pal.accent, 0.30);
    let accent_soft = Color::lerp(pal.background, pal.accent, 0.18);
    [
        Color::lerp(pal.background, pal.panel, s),
        Color::lerp(pal.panel, pal.background, s),
        Color::lerp(pal.background, accent_deep, 1.0 - s),
        Color::lerp(accent_soft, pal.panel, s),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        assert_eq!(
            Color::from_hex("#0a0a14").unwrap(),
            Color::from_rgb8(0x0a, 0x0a, 0x14)
        );
        assert_eq!(
            Color::from_hex("ff66aa").unwrap(),
            Color::from_rgb8(0xff, 0x66, 0xaa)
        );
        assert_eq!(
            Color::from_hex("#abc").unwrap(),
            Color::from_rgb8(0xaa, 0xbb, 0xcc)
        );
        assert_eq!(Color::from_hex("#ff66aa80").unwrap().a, 0x80);
    }

    #[test]
    fn hex_rejects_garbage() {
        assert!(Color::from_hex("#0a0a1").is_err());
        assert!(Color::from_hex("zzz").is_err());
        assert!(Color::from_hex("").is_err());
        assert!(Color::from_hex("#00ff0g").is_err());
    }

    #[test]
    fn midnight_matches_design_values() {
        let pal = midnight();
        assert_eq!(pal.background, Color::from_hex("#0a0a14").unwrap());
        assert_eq!(pal.accent, Color::from_hex("#ff66aa").unwrap());
        assert_eq!(pal.panel, Color::from_hex("#1a1a2e").unwrap());
        assert_eq!(pal.hit_300, Color::from_hex("#66eeff").unwrap());
        assert_eq!(pal.hit_miss, Color::from_hex("#ff4466").unwrap());
    }

    #[test]
    fn lerp_basics() {
        let a = Color::from_rgb8(0, 0, 0);
        let b = Color::from_rgb8(255, 255, 255);
        assert_eq!(Color::lerp(a, b, 0.0), a);
        assert_eq!(Color::lerp(a, b, 1.0), b);
        assert_eq!(Color::lerp(a, b, 0.5), Color::from_rgb8(128, 128, 128));
        assert_eq!(Color::lerp(a, b, 2.0), b, "t is clamped");
    }

    #[test]
    fn srgb_transfer_known_values() {
        assert!((srgb_to_linear(0.0) - 0.0).abs() < 1e-7);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-7);
        assert!((srgb_to_linear(0.5) - 0.21404114).abs() < 1e-6);
    }

    #[test]
    fn gradient_corners_is_periodic() {
        let pal = midnight();
        let a = gradient_corners(&pal, 0.0);
        let b = gradient_corners(&pal, GRADIENT_PERIOD_MS);
        let c = gradient_corners(&pal, 3.0 * GRADIENT_PERIOD_MS + 250.0);
        let d = gradient_corners(&pal, 250.0);
        for (x, y) in a.iter().zip(&b) {
            assert!((x.r as i32 - y.r as i32).abs() <= 1);
        }
        for (x, y) in c.iter().zip(&d) {
            assert!((x.r as i32 - y.r as i32).abs() <= 1);
        }
    }

    #[test]
    fn gradient_corners_stays_opaque_and_bounded() {
        let pal = midnight();
        for k in 0..=40 {
            let t = k as f64 * 337.0;
            for corner in gradient_corners(&pal, t) {
                assert_eq!(corner.a, 255);
            }
        }
        // huge t must not panic
        let _ = gradient_corners(&pal, 1.0e12);
    }
}
