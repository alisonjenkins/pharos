const float SEGMENTS = 8.0;

void main() {
    vec2 p = centred();
    float r = length(p);
    float slice = 2.0 * PI / SEGMENTS;
    float a = mod(atan(p.y, p.x) + u_time * 0.04, slice);
    a = abs(a - slice * 0.5);
    vec2 q = vec2(cos(a), sin(a)) * r;

    float bass = spectrum(0.04);
    float mid = spectrum(0.4);
    float high = spectrum(0.75);
    float v = sin(q.x * 16.0 - u_time * 0.9 + bass * 3.0) * cos(q.y * 24.0 + u_time * 0.5)
        + sin(r * 26.0 - u_time * 1.6 - mid * 4.0);
    float band = spectrum(fract(r * 1.4));
    float k = smoothstep(0.1, 0.9, 0.5 + 0.25 * v) * (0.3 + 0.7 * band);

    vec3 c = mix(u_bg, u_accent, k);
    c = mix(c, u_fg, pow(k, 5.0) * (0.25 + 0.5 * high));
    c *= 1.0 - 0.55 * smoothstep(0.3, 0.85, r);
    o_color = vec4(c, 1.0);
}
