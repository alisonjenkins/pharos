// Film grain over the background, with a faint accent bloom at the top.
void main() {
    vec2 uv = gl_FragCoord.xy / u_resolution;
    float frame = floor(u_time * 24.0);
    float g = hash(gl_FragCoord.xy + frame * 17.0) - 0.5;
    float bloom = smoothstep(0.4, 1.0, uv.y) * 0.08;
    vec3 c = tint(u_accent, bloom);
    c += g * 0.035;
    o_color = vec4(c, 1.0);
}
