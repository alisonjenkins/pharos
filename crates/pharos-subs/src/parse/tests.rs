#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::model::{Format, Script};

#[test]
fn srt_basic_with_tags_and_bom() {
    let src = "\u{feff}1\r\n00:00:01,500 --> 00:00:03,000\r\nHello <b>bold</b> &amp; <i>it</i>\r\nsecond\r\n\r\n2\r\n00:00:04,000 --> 00:00:05,250\r\n{\\an8}Top\r\n";
    let s = Script::parse(src);
    assert_eq!(s.format, Format::Srt);
    assert!(s.plain);
    assert_eq!(s.events.len(), 2);
    let e = &s.events[0];
    assert_eq!((e.start_ms, e.end_ms), (1500, 3000));
    assert_eq!(e.text, "Hello bold & it\nsecond");
    assert_eq!(e.runs.iter().filter(|r| r.ov.bold == Some(true)).count(), 1);
    assert_eq!(s.events[1].alignment, Some(8));
    assert_eq!(s.events[1].end_ms, 5250);
}

#[test]
fn srt_font_colour_maps_to_primary() {
    let s = Script::parse("1\n00:00:00,000 --> 00:00:01,000\n<font color=\"#ff8000\">x</font>y\n");
    let runs = &s.events[0].runs;
    assert_eq!(runs[0].ov.rgb[0], Some([0xff, 0x80, 0x00]));
    assert_eq!(runs[1].ov.rgb[0], None);
}

#[test]
fn vtt_skips_notes_and_strips_voice_tags() {
    let src = "WEBVTT\n\nNOTE hi\n\nSTYLE\n::cue{color:red}\n\nid1\n00:01.000 --> 00:02.000 align:start\n<v Bob><c.yellow>Hi</c> there</v>\n\n00:00:03.000 --> 00:00:04.000\n<i>x</i> {not ass}\n";
    let s = Script::parse(src);
    assert_eq!(s.format, Format::WebVtt);
    assert_eq!(s.events.len(), 2);
    assert_eq!(s.events[0].text, "Hi there");
    assert_eq!((s.events[0].start_ms, s.events[0].end_ms), (1000, 2000));
    assert_eq!(s.events[1].text, "x (not ass)");
}

const ASS: &str = "[Script Info]\nScriptType: v4.00+\nPlayResX: 1280\nPlayResY: 720\nWrapStyle: 2\nScaledBorderAndShadow: yes\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Liberation Sans,40,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,-1,0,0,0,100,100,0,0,1,3,1,2,20,20,30,1\nStyle: Sign,Arial,20,&H0000FF00,&H000000FF,&H00000000,&H00000000,0,1,0,0,100,100,0,0,1,0,0,8,10,10,10,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,Line one\\NLine two\\hX\\ntail\nDialogue: 2,0:00:03.00,0:00:04.00,Sign,,5,6,7,,{\\an5\\b1\\fs60\\c&H0000FF&\\1a&H80&\\bord4\\shad2\\fnImpact}Big{\\r}plain{\\pos(10,20)\\fscx120\\t(0,100,\\fs30)\\blur2}x\nDialogue: 0,garbage\n";

#[test]
fn ass_script_info_styles_events() {
    let s = Script::parse(ASS);
    assert_eq!(s.format, Format::Ass);
    assert!(!s.plain);
    assert_eq!((s.play_res_x, s.play_res_y, s.wrap_style), (1280, 720, 2));
    assert!(s.scaled_border_and_shadow);
    assert_eq!(s.styles.len(), 2);
    let d = &s.styles[0];
    assert_eq!(d.font, "Liberation Sans");
    assert!(d.bold && !d.italic);
    assert_eq!(d.colours[0], [255, 255, 255, 255]);
    assert_eq!(d.colours[3], [0, 0, 0, 127]);
    assert_eq!((d.margin_l, d.margin_v, d.alignment), (20, 30, 2));
    assert_eq!(s.styles[1].alignment, 8);
    assert_eq!(s.stats.bad_lines, 1);
    assert_eq!(s.events.len(), 2);
}

#[test]
fn ass_text_escapes_follow_wrap_style() {
    let s = Script::parse(ASS);
    // WrapStyle 2: \n is a hard break too; \h is a no-break space.
    assert_eq!(s.events[0].text, "Line one\nLine two\u{a0}X\ntail");
    let s1 = Script::parse(&ASS.replace("WrapStyle: 2", "WrapStyle: 0"));
    assert_eq!(s1.events[0].text, "Line one\nLine two\u{a0}X tail");
}

#[test]
fn ass_override_tags_apply_and_reset() {
    let s = Script::parse(ASS);
    let e = &s.events[1];
    assert_eq!(e.style, 1);
    assert_eq!((e.margin_l, e.margin_r, e.margin_v, e.layer), (5, 6, 7, 2));
    assert_eq!(e.alignment, Some(5));
    let r0 = &e.runs[0].ov;
    assert_eq!(r0.bold, Some(true));
    assert_eq!(r0.size, Some(60.0));
    assert_eq!(r0.rgb[0], Some([0xff, 0, 0]));
    assert_eq!(r0.alpha[0], Some(0x7f));
    assert_eq!((r0.bord, r0.shad), (Some(4.0), Some(2.0)));
    assert_eq!(r0.font.as_deref(), Some("Impact"));
    assert_eq!(&e.text[..e.runs[0].end], "Big");
    // \r cleared everything.
    assert_eq!(e.runs[1].ov, crate::model::Overrides::default());
}

#[test]
fn unknown_tags_are_ignored_and_counted() {
    let s = Script::parse(ASS);
    let u = &s.stats.unsupported_tags;
    assert_eq!(u.get("pos"), Some(&1));
    assert_eq!(u.get("fscx"), Some(&1));
    assert_eq!(u.get("t"), Some(&1));
    assert_eq!(u.get("blur"), Some(&1));
    // The `\fs30` nested inside `\t(...)` must not leak into the run.
    assert_eq!(s.events[1].runs[1].ov.size, None);
}

#[test]
fn ssa_v4_alignment_is_converted() {
    let src = "[Script Info]\nScriptType: v4.00\n[V4 Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\nStyle: Top,Arial,20,16777215,255,0,0,0,0,1,2,1,6,10,10,10,0,0\n[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: Marked=0,0:00:00.00,0:00:01.00,Top,,0,0,0,,hi\n";
    let s = Script::parse(src);
    assert_eq!(s.format, Format::Ssa);
    assert_eq!(s.styles[0].alignment, 8);
    assert_eq!(s.events.len(), 1);
}

#[test]
fn events_sorted_by_start() {
    let s = Script::parse(
        "1\n00:00:05,000 --> 00:00:06,000\nb\n\n2\n00:00:01,000 --> 00:00:02,000\na\n",
    );
    assert_eq!(s.events[0].text, "a");
}
