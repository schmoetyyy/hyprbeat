// hyprbeat phase-2 shader: ambient four-corner gradient + optional scale test
// card + music-synced pulse + latency flash + degraded-timing band. One
// uniform block, one fullscreen triangle, one draw call (§3.5).
//
// The uniform layout MUST match `render::Uniforms` (see render/mod.rs):
//   0   res_buf  vec2  buffer size in px
//   8   res_log  vec2  logical size in px
//   16  time_ms  f32   song/anim time (audio clock when a song is loaded)
//   20  flags    u32   bit0 = testcard, bit1 = timing degraded (I-12 stand-in)
//   24  level    f32   audio envelope 0..1 (mixer RMS — the visible beat)
//   28  flash    f32   latency-test flash 0..1
//   32  c00      vec4  top-left corner color
//   48  c10      vec4  top-right
//   64  c01      vec4  bottom-left
//   80  c11      vec4  bottom-right
//   96  accent   vec4  accent color

struct U {
    res_buf: vec2<f32>,
    res_log: vec2<f32>,
    time_ms: f32,
    flags: u32,
    level: f32,
    flash: f32,
    c00: vec4<f32>,
    c10: vec4<f32>,
    c01: vec4<f32>,
    c11: vec4<f32>,
    accent: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: U;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VOut {
    // fullscreen triangle
    var tri = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let p = tri[vi];
    var o: VOut;
    o.pos = vec4<f32>(p, 0.0, 1.0);
    o.uv = p * 0.5 + vec2<f32>(0.5, 0.5);
    return o;
}

@fragment
fn fs(v: VOut) -> @location(0) vec4<f32> {
    // Pixel coordinates, y-down from the top-left, in buffer pixels and in
    // logical units. Converting through the per-axis scale keeps the test
    // card aligned to logical pixels at ANY output scale (D-17/D-21).
    let uv = vec2<f32>(v.uv.x, 1.0 - v.uv.y);
    let px_buf = uv * u.res_buf;
    let scale = u.res_buf / max(u.res_log, vec2<f32>(1.0, 1.0));
    let px = px_buf / scale;
    let t = px / max(u.res_log, vec2<f32>(1.0, 1.0));

    // ambient gradient: bilinear blend of the four corners
    var col = mix(mix(u.c00, u.c10, t.x), mix(u.c01, u.c11, t.x), t.y);

    // music-synced pulse (phase 2 exit criterion): the corner colors are a
    // pure function of the audio clock time; the envelope lifts the whole
    // frame toward the accent with the loudness actually leaving the DAC.
    // Paused ⇒ silence ⇒ envelope 0 ⇒ the wallpaper visibly freezes with
    // the song.
    col = mix(col, u.accent, clamp(u.level, 0.0, 1.0) * 0.35);

    // latency-test flash: a brief bright overlay per tap
    col = mix(col, vec4<f32>(1.0, 1.0, 1.0, 1.0), clamp(u.flash, 0.0, 1.0) * 0.6);

    // degraded timing (I-12): pulsing amber band along the top edge — the
    // phase-2 stand-in for the HUD warning string (cosmic-text is phase 8).
    // Silent degradation is a bug, so the marker is unmissable.
    if (u.flags & 2u) != 0u {
        let pulse = 0.30 + 0.20 * sin(u.time_ms * 0.0062832); // ≈1 Hz
        let band_h = u.res_log.y * 0.035;
        if px.y < band_h {
            col = mix(col, vec4<f32>(1.0, 0.62, 0.12, 1.0), pulse);
        }
    }

    if (u.flags & 1u) != 0u {
        // 2-logical-px accent border: geometry must be exact at any scale
        let b = 2.0;
        let inside = px.x >= b && px.y >= b
            && px.x <= u.res_log.x - b && px.y <= u.res_log.y - b;
        if !inside {
            col = u.accent;
        }

        // 1-logical-px grid every 64 px
        let g = px % vec2<f32>(64.0, 64.0);
        if g.x < 1.0 || g.y < 1.0 {
            col = mix(col, vec4<f32>(1.0, 1.0, 1.0, 1.0), 0.22);
        }

        // 16-logical-px checker strip along the bottom edge
        if px.y > u.res_log.y - 96.0 {
            let c = (floor(px.x / 16.0) + floor(px.y / 16.0)) % 2.0;
            if c < 1.0 {
                col = mix(col, vec4<f32>(1.0, 1.0, 1.0, 1.0), 0.10);
            }
        }

        // concentric rings around the centre, one every 32 logical px
        let d = distance(px, u.res_log * 0.5);
        if abs(d % 32.0) < 1.5 {
            col = mix(col, vec4<f32>(1.0, 1.0, 1.0, 1.0), 0.30);
        }

        // 1 Hz sweep band: makes frame pacing visible at a glance
        let sweep = u.res_log.x * fract(u.time_ms / 1000.0);
        if abs(px.x - sweep) < 2.0 {
            col = mix(col, u.accent, 0.85);
        }
    }

    return vec4<f32>(col.rgb, 1.0);
}
