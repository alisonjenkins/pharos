// Slow curtains of the accent colour drifting along the top of the page.
void main() {
    vec2 uv = gl_FragCoord.xy / u_resolution;
    float aspect = u_resolution.x / u_resolution.y;
    float x = uv.x * aspect;
    float t = u_time * 0.05;

    float light = 0.0;
    for (int i = 0; i < 3; i++) {
        float fi = float(i);
        float centre = 0.82 - fi * 0.11
            + 0.05 * sin(x * 1.3 + t * 2.0 + fi * 2.1)
            + 0.07 * (fbm(vec2(x * 0.8 + fi * 7.0, t * 1.5)) - 0.5);
        float d = uv.y - centre;
        // Sharp lower edge, long soft fade upwards, like a real curtain.
        float curtain = d < 0.0 ? exp(-d * d * 900.0) : exp(-d * 7.0);
        float rays = 0.55 + 0.45 * fbm(vec2(x * 9.0 + fi * 3.0, t * 4.0));
        light += curtain * rays * (1.0 - fi * 0.25);
    }
    light *= smoothstep(0.25, 0.75, uv.y);

    vec3 c = tint(u_accent, light * 0.24);
    c = mix(c, u_fg, pow(clamp(light, 0.0, 1.0), 6.0) * 0.04);
    o_color = vec4(c, 1.0);
}
