// Columns of glyph cells falling in the accent colour, under faint scanlines.
// The glyphs are 3 by 5 random bit patterns that change as the head passes.
void main() {
    float cell = max(u_resolution.y / 46.0, 6.0);
    vec2 grid = gl_FragCoord.xy / cell;
    vec2 id = floor(grid);
    vec2 inner = fract(grid);
    float column = id.x;

    float speed = 0.35 + 0.65 * hash(vec2(column, 1.0));
    float rows = u_resolution.y / cell;
    float head = fract(u_time * speed * 0.12 + hash(vec2(column, 2.0))) * (rows + 24.0);
    float behind = head - (rows - id.y);
    float trail = behind > 0.0 ? exp(-behind * 0.18) : 0.0;
    float lit = step(0.35, hash(vec2(column, 3.0)));

    float tick = floor(u_time * (2.0 + 6.0 * hash(id)));
    vec2 bit = floor(inner * vec2(3.0, 5.0));
    float glyph = step(0.45, hash(id * 7.0 + bit + tick * 0.13));
    glyph *= step(0.12, inner.x) * step(inner.x, 0.88) * step(0.08, inner.y) * step(inner.y, 0.92);

    float head_glow = behind > 0.0 && behind < 1.0 ? 1.0 : 0.0;
    float amount = glyph * lit * (trail * 0.22 + head_glow * 0.06);
    vec3 c = tint(u_accent, amount);
    c = mix(c, u_fg, glyph * lit * head_glow * 0.12);
    c *= 0.94 + 0.06 * sin(gl_FragCoord.y * PI);
    o_color = vec4(c, 1.0);
}
