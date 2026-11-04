const float HORIZON = 0.38;
const float SUN = 0.2;

void main() {
    vec2 uv = gl_FragCoord.xy / u_resolution;
    float aspect = u_resolution.x / u_resolution.y;
    float above = uv.y - HORIZON;

    // Sky: the accent glows up from the horizon.
    vec3 c = mix(u_bg, mix(u_bg, u_accent, 0.35), exp(-max(above, 0.0) * 5.0));

    // A striped sun, the stripes widening towards the horizon.
    vec2 s = vec2((uv.x - 0.5) * aspect, above - SUN * 0.75);
    float disc = smoothstep(SUN, SUN - 0.004, length(s));
    float below = clamp((0.35 * SUN - s.y) / (1.35 * SUN), 0.0, 1.0);
    float band = fract(s.y * 22.0 - u_time * 0.4);
    float cut = step(0.6 * below, band);
    vec3 sun = mix(u_accent, u_fg, smoothstep(-SUN, SUN, s.y) * 0.75);
    c = mix(c, sun, disc * cut * (0.85 + 0.15 * u_level));

    // Mountains: the spectrum, treble low in the middle under the sun and
    // the bass rising to the edges.
    float x = abs(uv.x - 0.5) * 2.0;
    float peak = pow(spectrum(1.0 - x), 1.2) * 0.26 * (0.35 + 0.65 * x);
    float ridge = HORIZON + peak;
    float hill = step(0.0, above) * step(uv.y, ridge);
    float edge = exp(-abs(uv.y - ridge) * 260.0) * step(0.0, above);
    c = mix(c, mix(u_bg, u_accent, 0.12), hill);
    c += u_accent * edge * 0.8;

    // Floor: a perspective grid rolling towards the viewer.
    // Derivatives need uniform control flow, so take them for every pixel;
    // the clamp keeps the unused values finite above the horizon.
    float depth = 0.09 / max(-above, 1e-4);
    vec2 g = vec2((uv.x - 0.5) * aspect * depth * 1.4, depth + u_time * 0.6);
    // Distance to the nearest line in pixels, so lines stay thin far off.
    vec2 px = abs(fract(g + 0.5) - 0.5) / fwidth(g);
    if (above < 0.0) {
        float grid = (1.0 - min(min(px.x, px.y), 1.0)) * smoothstep(9.0, 1.0, depth);
        c = u_bg * 0.8 + u_accent * grid * (0.55 + 0.45 * u_level);
        c += u_accent * 0.3 * exp(above * 30.0);
    }
    o_color = vec4(c, 1.0);
}
