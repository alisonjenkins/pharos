// A CRT: fine scanlines, a bright band rolling down, and a glow from the top.
void main() {
    vec2 uv = gl_FragCoord.xy / u_resolution;
    float lines = 0.5 + 0.5 * sin(gl_FragCoord.y * PI * 0.66);
    float roll = fract(1.0 - u_time * 0.04);
    float d = (uv.y - roll) * 14.0;
    float band = exp(-d * d);
    float glow = smoothstep(0.35, 1.0, uv.y);
    float flicker = 0.97 + 0.03 * noise(vec2(u_time * 8.0, 0.0));

    float amount = (glow * 0.10 + band * 0.07) * flicker;
    vec3 c = tint(u_accent, amount);
    c *= 0.9 + 0.1 * lines;
    o_color = vec4(c, 1.0);
}
