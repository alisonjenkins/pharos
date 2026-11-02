const float BARS = 40.0;
const float FLOOR = 0.2;

void main() {
    vec2 uv = gl_FragCoord.xy / u_resolution;
    float x = abs(uv.x - 0.5) * 2.0;
    float cell = fract(x * BARS);
    float h = pow(spectrum((floor(x * BARS) + 0.5) / BARS), 1.3) * 0.72 + 0.008;
    float gap = smoothstep(0.12, 0.2, cell) * smoothstep(0.88, 0.8, cell);
    float y = uv.y - FLOOR;

    float bar = gap * step(0.0, y) * step(y, h);
    float tip = gap * step(h, y) * exp(-(y - h) * 60.0) * h;
    float mirror = gap * step(y, 0.0) * step(-h * 0.4, y) * (1.0 + y / (h * 0.4)) * 0.22;

    vec3 colour = mix(u_accent, u_fg, smoothstep(0.0, 1.0, y / h));
    vec3 c = u_bg * (0.85 + 0.15 * uv.y);
    c = mix(c, colour, bar);
    c += u_accent * (tip * 0.9 + mirror);
    c += u_accent * 0.1 * u_level * smoothstep(0.6, 0.0, abs(uv.y - FLOOR));
    o_color = vec4(c, 1.0);
}
