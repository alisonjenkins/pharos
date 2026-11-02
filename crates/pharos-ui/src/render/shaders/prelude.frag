#version 300 es
precision highp float;

uniform float u_time;
uniform vec2 u_resolution;
uniform vec3 u_bg;
uniform vec3 u_accent;
uniform vec3 u_fg;
uniform float u_level;
uniform sampler2D u_data;

out vec4 o_color;

const float PI = 3.14159265;

// Spectrum energy at x (0 = bass, 1 = treble), 0 to 1.
float spectrum(float x) {
    return texture(u_data, vec2(clamp(x, 0.0, 1.0), 0.25)).r;
}

// Waveform sample at x, -1 to 1.
float wave(float x) {
    return texture(u_data, vec2(clamp(x, 0.0, 1.0), 0.75)).r * 2.0 - 1.0;
}

// Pixel coordinates centred and scaled so the short side spans -0.5 to 0.5.
vec2 centred() {
    return (gl_FragCoord.xy - 0.5 * u_resolution) / min(u_resolution.x, u_resolution.y);
}
