const float LAYERS = 5.0;

void main() {
    vec2 p = centred();
    vec3 c = u_bg * (1.0 - 0.35 * length(p));
    for (float i = 0.0; i < LAYERS; i += 1.0) {
        // Each layer flies from far to near and starts again.
        float z = fract(i / LAYERS + u_time * 0.06);
        float scale = mix(18.0, 0.8, z);
        vec2 q = p * scale + i * 13.7;
        vec2 cell = floor(q);
        vec2 f = fract(q) - 0.5;
        vec2 at = vec2(hash(cell), hash(cell + 7.1)) - 0.5;
        float d = length(f - at * 0.7);
        float twinkle = spectrum(hash(cell + 3.3));
        float size = 0.02 + 0.05 * twinkle;
        float star = smoothstep(size, 0.0, d) * smoothstep(0.0, 0.3, z) * smoothstep(1.0, 0.8, z);
        vec3 tint = mix(u_fg, u_accent, hash(cell + 1.9));
        c += tint * star * (0.5 + 1.2 * twinkle + 0.5 * u_level);
    }
    c += u_accent * 0.12 * u_level * exp(-dot(p, p) * 6.0);
    o_color = vec4(c, 1.0);
}
