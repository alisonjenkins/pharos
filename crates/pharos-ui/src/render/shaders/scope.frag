// The waveform row peaks near 0.9, so the trace stays inside the middle
// two thirds of the screen however loud the song is.
const float AMPLITUDE = 0.36;

float trace(float x) {
    return wave(x) * AMPLITUDE;
}

void main() {
    vec2 uv = gl_FragCoord.xy / u_resolution;
    float dx = 1.0 / u_resolution.x;
    // Distance to the curve in pixels, corrected for its slope so steep
    // edges stay as thick as flat ones.
    float slope = (trace(uv.x + dx) - trace(uv.x - dx)) * u_resolution.y * 0.5;
    float d = abs(uv.y - 0.5 - trace(uv.x)) * u_resolution.y / sqrt(1.0 + slope * slope);

    float core = exp(-d * d / 4.0);
    float halo = exp(-d / 22.0) * (0.35 + u_level);
    vec2 grid = abs(fract(uv * vec2(10.0, 6.0)) - 0.5);
    float lines = step(0.49, max(grid.x, grid.y)) * 0.06;
    float vignette = 1.0 - 0.5 * length(uv - 0.5);

    vec3 c = u_bg * vignette + u_fg * lines;
    c += u_accent * halo * 0.55 + mix(u_accent, u_fg, 0.6) * core;
    o_color = vec4(c, 1.0);
}
