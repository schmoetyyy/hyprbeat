//! Pure scaling math for compositor-driven output scale (D-17, I-11).
//!
//! All functions here are deterministic and I/O-free (I-09): the buffer
//! rounding rules for fractional scale are exactly the kind of thing that
//! must never regress silently, so they are pinned by unit tests.

/// Buffer size for a logical surface size at a given (possibly fractional)
/// compositor scale.
///
/// This is the D-21 rule: render at `round(logical × scale)` and map the
/// buffer back onto the logical size with `wp_viewporter`, which is the
/// pixel-correct way to do fractional scaling on Wayland.
pub fn buffer_size(logical: (u32, u32), scale: f64) -> (u32, u32) {
    // Nonsense from the wire degrades to 1× rather than panicking — a
    // wallpaper must never die on a weird compositor (I-08 posture).
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let w = ((logical.0 as f64) * scale).round().max(1.0) as u32;
    let h = ((logical.1 as f64) * scale).round().max(1.0) as u32;
    (w, h)
}

/// `wp_fractional_scale_v1` reports the preferred scale in units of 1/120
/// (e.g. 150 means 1.25×).
pub fn factor_from_fractional_units(units: u32) -> f64 {
    units as f64 / 120.0
}

/// Defensive clamp for whatever scale a compositor reports. The *user-facing*
/// `[render] scale` clamp (D-17) is a separate concern landing in phase 5;
/// this one only guards against nonsense from the wire.
pub fn clamp_scale(scale: f64) -> f64 {
    if !scale.is_finite() {
        return 1.0;
    }
    scale.clamp(0.25, 4.0)
}

/// Frame gate for the idle throttle (§3.5 `target_fps_idle`): render only if
/// at least `min_interval_ms` has passed since the last rendered frame.
///
/// `since_last_render_ms` is a **relative duration** (how long ago the last
/// render finished), NOT an absolute timestamp — the two were mixed up in the
/// first live run (2026-11-12) and froze the wallpaper after two frames: the
/// gate silently collapsed to "was the last render's absolute time ≥ interval",
/// nonsense that is false forever when the early renders happen < interval ms
/// after startup. Relative-only signature makes that mix-up unrepresentable.
pub fn should_render(since_last_render_ms: Option<f64>, min_interval_ms: f64) -> bool {
    match since_last_render_ms {
        None => true,
        Some(since) => since >= min_interval_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_scales_are_exact() {
        assert_eq!(buffer_size((1920, 1080), 1.0), (1920, 1080));
        assert_eq!(buffer_size((1920, 1080), 2.0), (3840, 2160));
    }

    #[test]
    fn fractional_125() {
        // the phase-1 exit criterion number
        assert_eq!(buffer_size((1920, 1080), 1.25), (2400, 1350));
        assert_eq!(buffer_size((2560, 1440), 1.25), (3200, 1800));
    }

    #[test]
    fn fractional_rounding_half_up() {
        // 1366 × 1.25 = 1707.5 → 1708
        assert_eq!(buffer_size((1366, 768), 1.25), (1708, 960));
        // 1512 × 1.25 = 1890
        assert_eq!(buffer_size((1512, 982), 1.25), (1890, 1228));
        // 100 × 1.1 = 110.0000... → 110
        assert_eq!(buffer_size((100, 100), 1.1), (110, 110));
    }

    #[test]
    fn tiny_sizes_never_collapse() {
        assert_eq!(buffer_size((1, 1), 0.5), (1, 1));
        assert_eq!(buffer_size((2, 2), 0.25), (1, 1));
    }

    #[test]
    fn nonsensical_scale_falls_back_to_one() {
        assert_eq!(buffer_size((100, 100), f64::NAN), (100, 100));
        assert_eq!(buffer_size((100, 100), 0.0), (100, 100));
    }

    #[test]
    fn fractional_units() {
        assert_eq!(factor_from_fractional_units(120), 1.0);
        assert_eq!(factor_from_fractional_units(150), 1.25);
        assert_eq!(factor_from_fractional_units(240), 2.0);
    }

    #[test]
    fn clamp_scale_guards() {
        assert_eq!(clamp_scale(1.25), 1.25);
        assert_eq!(clamp_scale(0.01), 0.25);
        assert_eq!(clamp_scale(9.0), 4.0);
        assert_eq!(clamp_scale(f64::NAN), 1.0);
    }

    #[test]
    fn frame_gate() {
        assert!(should_render(None, 33.3), "first frame always renders");
        assert!(!should_render(Some(10.0), 33.3), "too soon");
        assert!(should_render(Some(33.3), 33.3), "interval elapsed");
        assert!(should_render(Some(33.4), 33.3), "interval just elapsed");
        // the first-live-run regression: an early render must NOT poison the
        // gate for the rest of the session (absolute/relative mix-up)
        assert!(
            should_render(Some(5000.0), 33.3),
            "long after the last render it must be due again"
        );
    }
}
