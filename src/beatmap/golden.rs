//! File-level tests over the committed golden corpus (`assets/golden/*.osu`).
//!
//! The corpus is synthetic (I-03): invented metadata, placeholder audio
//! filenames, nothing playable. All filesystem I/O lives here, inside
//! `#[cfg(test)]` — `parse_osu` itself stays zero-I/O (I-09). Real-world
//! hardening runs at the phase-3 exit validation with user-supplied maps.

use super::*;

/// Corpus root: `<crate>/assets/golden`, resolved the same way on every
/// machine (tests run from the manifest dir, never the process CWD).
const GOLDEN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/golden");

/// Read one corpus fixture.
fn golden(name: &str) -> String {
    std::fs::read_to_string(format!("{GOLDEN_DIR}/{name}"))
        .expect("golden fixture is committed under assets/golden")
}

/// Parse one corpus fixture; panics with the parse error on failure.
fn parse_golden(name: &str) -> Beatmap {
    parse_osu(&golden(name)).unwrap()
}

#[test]
fn corpus_has_exactly_the_expected_files() {
    let mut names: Vec<String> = std::fs::read_dir(GOLDEN_DIR)
        .expect("assets/golden is committed")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "bom-crlf-comments.osu",
            "curves-and-edges.osu",
            "invalid-line.osu",
            "unsubmitted.osu",
            "v14-lazer.osu",
            "v14-standard.osu",
            "v6-legacy.osu",
        ]
    );
}

#[test]
fn v14_standard_parses_end_to_end() {
    let map = parse_golden("v14-standard.osu");
    assert_eq!(map.format_version, 14);
    assert_eq!(map.general.audio_filename, "golden-standard.mp3");
    assert_eq!(map.general.audio_lead_in_ms, 800.0);
    assert_eq!(map.general.stack_leniency, 0.7);
    assert_eq!(map.general.mode, 0);
    assert_eq!(map.metadata.title, "Golden Standard");
    assert_eq!(map.metadata.title_unicode, "Gölden Standard");
    assert_eq!(map.metadata.artist_unicode, "hyprbëat");
    assert_eq!(map.metadata.version, "Standard");
    assert_eq!(map.metadata.beatmap_id, 4242);
    assert_eq!(map.metadata.beatmap_set_id, 742);
    assert_eq!(map.difficulty.hp_drain_rate, 6.0);
    assert_eq!(map.difficulty.circle_size, 4.0);
    assert_eq!(map.difficulty.overall_difficulty, 7.5);
    assert_eq!(map.difficulty.approach_rate, 9.2);
    assert_eq!(map.difficulty.slider_multiplier, 1.9);
    assert_eq!(map.difficulty.slider_tick_rate, 2.0);
    // [Editor], [Events] and [Colours] are recorded in file order.
    assert_eq!(map.skipped_sections, vec!["Editor", "Events", "Colours"]);

    // 0,375… uninherited anchor; -100/-50/-125 inherited SV points; one
    // kiai window via effects bit 0; effects 8/9 toggle the bar-line omit.
    assert_eq!(map.timing_points.len(), 5);
    assert!(map.timing_points[0].uninherited);
    assert_eq!(map.timing_points[0].beat_length, 375.0);
    assert!(!map.timing_points[0].kiai);
    assert_eq!(map.timing_points[1].sv_multiplier(), 1.0);
    assert!(map.timing_points[2].kiai);
    assert_eq!(map.timing_points[2].sv_multiplier(), 2.0);
    assert!(map.timing_points[3].uninherited);
    assert!(!map.timing_points[3].kiai);
    assert!(map.timing_points[3].omit_first_bar_line);
    assert!(map.timing_points[4].kiai);
    assert!(map.timing_points[4].omit_first_bar_line);
    assert_eq!(map.timing_points[4].sv_multiplier(), 0.8);

    let circles = map
        .hit_objects
        .iter()
        .filter(|o| matches!(o.kind, HitObjectKind::Circle))
        .count();
    let sliders = map
        .hit_objects
        .iter()
        .filter(|o| matches!(o.kind, HitObjectKind::Slider(_)))
        .count();
    let spinners = map
        .hit_objects
        .iter()
        .filter(|o| matches!(o.kind, HitObjectKind::Spinner { .. }))
        .count();
    assert_eq!((circles, sliders, spinners), (4, 4, 2));

    assert_eq!(map.hit_objects[0].time_ms, 0.0);
    assert!(map.hit_objects[0].new_combo);
    assert_eq!(map.hit_objects[0].hit_sample, "0:0:0:0:");
    assert_eq!(map.hit_objects[1].hit_sound, 2);
    assert_eq!(map.hit_objects[8].combo_colour_skip, 1);

    // Bézier with a duplicate control point (segment break), plus the raw
    // edge-sound pipe fields.
    let HitObjectKind::Slider(s) = &map.hit_objects[3].kind else {
        panic!("expected a slider");
    };
    assert_eq!(s.curve_type, CurveType::Bezier);
    assert_eq!(
        s.curve_points,
        vec![(350.0, 200.0), (350.0, 200.0), (400.0, 300.0)]
    );
    assert_eq!(s.edge_sounds, "2|2");
    assert_eq!(s.edge_sets, "0:0|0:2");

    let HitObjectKind::Spinner { end_time_ms } = map.hit_objects[4].kind else {
        panic!("expected a spinner");
    };
    assert_eq!(end_time_ms, 4200.0);
    assert_eq!(map.hit_objects[4].hit_sample, "3:0:0:70:");
    let HitObjectKind::Spinner { end_time_ms } = map.hit_objects[9].kind else {
        panic!("expected a spinner");
    };
    assert_eq!(end_time_ms, 8500.0);
    assert_eq!(map.hit_objects[9].hit_sample, DEFAULT_HIT_SAMPLE);
}

#[test]
fn v14_lazer_version_is_kept() {
    let map = parse_golden("v14-lazer.osu");
    assert_eq!(map.format_version, 128);
    assert_eq!(map.general.mode, 0);
    assert_eq!(map.metadata.beatmap_id, 4243);
    assert_eq!(map.difficulty.approach_rate, 10.0);
    assert_eq!(map.hit_objects.len(), 1);
    // `WidescreenStoryboard` is a real key the v0.1 parser ignores — an
    // unknown KEY is leniency, not a skipped SECTION.
    assert!(map.skipped_sections.is_empty());
}

#[test]
fn v6_legacy_defaults_and_fallbacks() {
    let map = parse_golden("v6-legacy.osu");
    assert_eq!(map.format_version, 6);
    // No `Mode` key → defaults to 0; no ids → default to 0.
    assert_eq!(map.general.mode, 0);
    assert_eq!(map.metadata.beatmap_id, 0);
    assert_eq!(map.metadata.beatmap_set_id, 0);
    assert_eq!(map.general.stack_leniency, 0.2);
    assert_eq!(map.general.audio_lead_in_ms, 1200.0);
    // Pre-v8 files carry no ApproachRate — falls back to OverallDifficulty.
    assert_eq!(
        map.difficulty.approach_rate,
        map.difficulty.overall_difficulty
    );
    assert_eq!(map.difficulty.approach_rate, 6.0);
    assert_eq!(map.skipped_sections, vec!["Events"]);
    assert_eq!(map.timing_points.len(), 2);
    for tp in &map.timing_points {
        assert!(tp.uninherited);
        // 7-field legacy lines carry no effects field at all.
        assert!(!tp.kiai);
        assert!(!tp.omit_first_bar_line);
    }
    let HitObjectKind::Slider(s) = &map.hit_objects[2].kind else {
        panic!("expected a slider");
    };
    assert_eq!(s.curve_type, CurveType::Linear);
    assert_eq!(s.curve_points, vec![(400.0, 120.0)]);
    assert_eq!(map.hit_objects.len(), 3);
}

#[test]
fn bom_crlf_and_comments_survive_on_disk() {
    let raw = golden("bom-crlf-comments.osu");
    assert!(raw.starts_with('\u{feff}'), "fixture must keep its BOM");
    assert!(raw.contains("\r\n"), "fixture must keep CRLF endings");
    let map = parse_osu(&raw).unwrap();
    assert_eq!(map.metadata.title, "Golden Messy");
    assert_eq!(map.difficulty.approach_rate, 9.5);
    assert_eq!(map.timing_points.len(), 1);
    assert_eq!(map.hit_objects.len(), 2);
    assert_eq!(map.hit_objects[0].time_ms, 500.0);
    assert_eq!(map.hit_objects[1].time_ms, 900.0);
}

#[test]
fn curves_and_edges_pin_the_slider_grammar() {
    let map = parse_golden("curves-and-edges.osu");
    assert_eq!(map.timing_points[1].sv_multiplier(), 0.4);
    assert_eq!(map.timing_points[2].sv_multiplier(), 2.5);
    assert!(map.timing_points[2].kiai);

    let sliders: Vec<&Slider> = map
        .hit_objects
        .iter()
        .map(|o| match &o.kind {
            HitObjectKind::Slider(s) => s,
            _ => panic!("only sliders in this fixture"),
        })
        .collect();
    assert_eq!(sliders.len(), 6);
    assert_eq!(
        sliders.iter().map(|s| s.curve_type).collect::<Vec<_>>(),
        vec![
            CurveType::Bezier,
            CurveType::Bezier,
            CurveType::Catmull,
            CurveType::Linear,
            CurveType::Perfect,
            CurveType::Perfect,
        ]
    );
    // A repeated control point is a bézier segment break — preserved.
    assert_eq!(
        sliders[1].curve_points,
        vec![(100.0, 50.0), (100.0, 50.0), (200.0, 50.0)]
    );
    // Full slider grammar: slides, length, edgeSounds, edgeSets.
    assert_eq!(sliders[3].slides, 2);
    assert_eq!(sliders[3].edge_sounds, "1|2|1");
    assert_eq!(sliders[3].edge_sets, "0:0|0:2|0:0");
    assert_eq!(sliders[4].edge_sounds, "2|0");
    assert_eq!(sliders[4].edge_sets, "0:0|0:2");
    assert_eq!(map.hit_objects[4].hit_sample, "0:0:0:100:");
    // Wiki: P with more than 3 points degrades to bézier — phase 6 (§7.3)
    // consumes that; the parser keeps the raw anchors and the P type.
    assert_eq!(sliders[5].curve_type, CurveType::Perfect);
    assert_eq!(sliders[5].curve_points.len(), 3);
}

#[test]
fn unsubmitted_and_lenient_timing_fields() {
    let map = parse_golden("unsubmitted.osu");
    assert_eq!(map.metadata.beatmap_id, -1);
    assert_eq!(map.metadata.beatmap_set_id, -1);
    // No StackLeniency key → the legacy default.
    assert_eq!(map.general.stack_leniency, DEFAULT_STACK_LENIENCY);
    let tp = &map.timing_points[0];
    assert!(tp.uninherited);
    assert_eq!(tp.meter, 4);
    assert_eq!(tp.volume, 60.0);
    assert_eq!(map.hit_objects.len(), 3);
}

#[test]
fn invalid_line_reports_the_physical_line_number() {
    let raw = golden("invalid-line.osu");
    assert!(raw.starts_with('\u{feff}'), "fixture must keep its BOM");
    let e = parse_osu(&raw).unwrap_err();
    // Line 22, counted across the BOM, blank lines and comments.
    assert_eq!(e.line_no, 22);
    assert_eq!(e.line, "256,192,1200,not-an-object");
    assert!(e.reason.contains("at least 5"), "reason: {}", e.reason);
    let text = e.to_string();
    assert!(text.contains("line 22"), "display: {text}");
    assert!(text.contains("not-an-object"), "display: {text}");
}
