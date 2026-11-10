# ADR-0088: 3D in VR is a WebXR media layer

- **Status:** Proposed
- **Date:** 2026-11-09T19:30:00Z
- **Deciders:** Alison Jenkins

## Context

Spec 022 US3 asks for a packed 3D version (side-by-side or top-and-bottom)
to play in stereo on a headset: a Meta Quest now, a Steam Frame later. Both
run a browser, so pharos-ui can do this itself with WebXR instead of handing
the file to a VR player app.

A packed frame has two ways into a headset's eyes:

1. **A WebXR media layer.** `XRMediaBinding.createQuadLayer(video, {layout})`
   with `stereo-left-right` or `stereo-top-bottom` hands the `<video>` to the
   compositor, which draws each half into its eye at the display's own
   refresh. The page copies no frame. Quest Browser supports it.
2. **A WebGL quad per eye.** Upload each video frame as a texture and draw
   its left half for the left eye and its right half for the right, through
   the `render/` WebGL2 layer (ADR-0039). That works in any WebXR browser, but
   copies every frame and redraws both eyes every display frame, so it costs
   GPU time and some sharpness.

WebXR is not in web-sys's stable bindings. Turning on
`--cfg=web_sys_unstable_apis` changes RUSTFLAGS for the whole crate graph,
which rebuilds every crate on the already saturated CI node.

## Decision

We will show 3D in VR as a WebXR media quad layer where the browser has one,
and as a WebGL quad per eye (option 2) where it does not. The session is
requested with `optionalFeatures: ["layers"]` from the
click on "Watch in VR", with nothing awaited before it, so the click's user
activation holds. The screen is 3.5 m wide, 3 m in front of the viewer in a
`local` reference space, and has the eye's shape (`stereo::eye_size`). The
controller's select toggles play through the player's own control (a group's
play stays the group's), and squeeze ends the session. The button shows only
when `navigator.xr.isSessionSupported("immersive-vr")` resolves true and the
version is SBS or TAB (V923).

WebXR is reached through `js_sys::Reflect` in `native/xr_web.rs`, not
web-sys's unstable bindings.

The layers path runs when the session granted `layers` and
`XRMediaBinding` exists. Otherwise an `XRWebGLLayer` on an `xrCompatible`
WebGL2 context gets one quad per view (`render/webgl_xr.rs`), placed where
the media layer would be and sampling that eye's part of the frame
(`stereo::eye_rect`). The frame is uploaded when
`requestVideoFrameCallback` reports a new one, so a 90 Hz display showing
24 fps uploads 24 times a second, not 90. A browser that never reports
frames during the session gets an upload every display frame instead.
Added 2026-11-09 so a headset browser without layers (possibly the Steam
Frame's) still shows stereo.

## Consequences

- The Quest draws 3D at full quality with no page work per frame.
- A headset browser without WebXR layers shows stereo through the WebGL
  path, at the cost of one texture upload per video frame and a redraw of
  both eyes per display frame, and a little sharpness from resampling.
- Outcomes are console lines for now (`pharos: xr session started` or
  `failed reason=…`, each with `packing`, `layout`, `path`, `layers-granted`
  and `media-binding`, so a wrong path can be traced to its cause). The 016 telemetry
  events need a schema bump, a separate change.
- Playwright has no WebXR, so the stereo path is checked by hand on a Quest.
  CI checks only that the button is absent without WebXR.
