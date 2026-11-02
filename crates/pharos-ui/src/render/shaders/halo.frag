const float RADIUS = 0.2;
const float RAYS = 18.0;

void main() {
    vec2 p = centred();
    float r = length(p);
    float a = atan(p.y, p.x);
    float s = spectrum(abs(a) / PI);

    float ring = RADIUS + 0.04 * u_level + 0.16 * s;
    float d = abs(r - ring);
    float line = exp(-d * d * 6000.0);
    float glow = exp(-d * 16.0) * (0.25 + s);
    float outside = smoothstep(ring, ring + 0.05, r) * (1.0 - smoothstep(ring + 0.05, 0.75, r));
    float rays = pow(max(cos(a * RAYS + u_time * 0.3), 0.0), 30.0) * outside * s;
    float core = exp(-r * r * 50.0) * (0.15 + 0.85 * u_level);

    vec3 c = u_bg * (1.0 - 0.5 * smoothstep(0.3, 1.0, r));
    c += u_accent * (glow * 0.6 + rays * 0.6 + core * 0.8) + u_fg * line * 0.9;
    o_color = vec4(c, 1.0);
}
