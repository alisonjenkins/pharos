const float RADIUS = 0.24;
const float RINGS = 3.0;

void main() {
    vec2 p = centred();
    float r = length(p);
    // Mirrored around the vertical, so the ring has no seam.
    float x = abs(atan(p.x, p.y)) / PI;

    float core = 0.0;
    float glow = 0.0;
    for (float i = 0.0; i < RINGS; i += 1.0) {
        float fade = 1.0 - i / RINGS;
        float ring = RADIUS * (1.0 + 0.22 * i) + wave(fract(x + i * 0.31)) * 0.07 * fade;
        float d = abs(r - ring);
        core += exp(-d * d * 12000.0) * fade;
        glow += exp(-d * 30.0) * fade * fade;
    }
    float bass = spectrum(0.04);
    float heart = exp(-r * r * 40.0) * (0.1 + 0.6 * bass);

    vec3 c = u_bg * (1.0 - 0.5 * smoothstep(0.25, 0.9, r));
    c += u_accent * (glow * (0.25 + 0.35 * u_level) + heart) + mix(u_accent, u_fg, 0.6) * core;
    o_color = vec4(c, 1.0);
}
