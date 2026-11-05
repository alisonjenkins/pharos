use pharos_subs::{Config, Frame, Renderer, Script, Viewport};

pub const FONT: &[u8] = include_bytes!("../fonts/LiberationSans-Regular.ttf");

pub fn renderer_with(script: &str) -> Renderer {
    let mut r = Renderer::new(Config::default());
    r.add_font(FONT.to_vec()).unwrap();
    r.load(Script::parse(script));
    r
}

pub const TWO_LINE_SRT: &str =
    "1\n00:00:01,000 --> 00:00:03,000\nFirst line of the cue\nSecond, longer line of the cue\n\n\
     2\n00:00:05,000 --> 00:00:06,000\nNext\n";

pub fn vp() -> Viewport {
    Viewport::full(1920, 1080)
}

/// FNV-1a over everything a compositor would see.
pub fn hash_frame(f: &Frame) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: &[u8]| {
        for x in b {
            h ^= u64::from(*x);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for m in &f.masks {
        eat(&m.x.to_le_bytes());
        eat(&m.y.to_le_bytes());
        eat(&m.w.to_le_bytes());
        eat(&m.h.to_le_bytes());
        eat(&m.tint);
        eat(&m.coverage);
    }
    h
}
