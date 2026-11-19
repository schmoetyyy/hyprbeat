// hyprbeat visualizer shader: instanced solid quads (D-05 — the geometry
// + per-instance-color half; the texture-atlas half lands with the
// gameplay sprites in phase 4). One screen uniform, one instance buffer,
// one draw call per layer batch (§3.5).
//
// The instance layout MUST match `render::pipeline::Quad` (see
// render/pipeline.rs):
//   0   pos    vec2  top-left corner in logical px (y-down)
//   8   size   vec2  extent in logical px
//   16  color  vec4  straight (non-premultiplied) alpha; RGB already
//                    converted for the target format on the CPU
//                    (render::color_for_format — the phase-1 discipline)

struct Screen {
    res_log: vec2<f32>,
};

@group(0) @binding(0) var<uniform> screen: Screen;

struct Instance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Instance) -> VOut {
    // unit square as a 6-vertex triangle list
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let c = corners[vi];
    let px = inst.pos + c * inst.size;
    // logical px (y-down from the top-left) -> NDC (y-up); the max()
    // guards a zero-size surface the same way the phase-1 shader does
    let r = max(screen.res_log, vec2<f32>(1.0, 1.0));
    let ndc = vec2<f32>(px.x / r.x * 2.0 - 1.0, 1.0 - px.y / r.y * 2.0);
    var o: VOut;
    o.pos = vec4<f32>(ndc, 0.0, 1.0);
    o.color = inst.color;
    return o;
}

@fragment
fn fs(v: VOut) -> @location(0) vec4<f32> {
    // blending (src: alpha, dst: 1-src-alpha) is set on the pipeline;
    // the shader only passes the instance color through
    return v.color;
}
