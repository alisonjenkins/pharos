void main() {
    vec2 p = centred();
    float bass = spectrum(0.03);
    float t = u_time * 0.2;
    vec2 orbit = vec2(sin(t * 1.1), cos(t * 0.7)) * 0.3;

    float v = sin(p.x * 5.0 + t * 3.0)
        + sin(p.y * 4.0 - t * 2.0)
        + sin((p.x + p.y) * 3.5 + t)
        + sin(length(p - orbit) * 9.0 - t * 4.0 - bass * 3.0);
    float k = 0.5 + 0.5 * sin(v * PI * 0.5 + u_level * 1.5);

    vec3 c = mix(u_bg, u_accent, k * (0.45 + 0.4 * bass));
    c = mix(c, u_fg, pow(k, 8.0) * 0.3);
    c *= 1.0 - 0.45 * smoothstep(0.35, 0.9, length(p));
    o_color = vec4(c, 1.0);
}
