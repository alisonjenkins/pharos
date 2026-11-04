#version 300 es
// A real array at location 0: without one, desktop OpenGL (macOS) makes the
// browser emulate attribute 0 on every draw.
layout(location = 0) in vec2 a_corner;
void main() {
    gl_Position = vec4(a_corner * 2.0 - 1.0, 0.0, 1.0);
}
