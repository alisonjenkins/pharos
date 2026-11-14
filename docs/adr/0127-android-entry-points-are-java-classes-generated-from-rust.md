# ADR-0127: Android entry points are Java classes generated from Rust

- **Status:** Accepted
- **Date:** 2026-11-14T08:50:00Z
- **Deciders:** Alison (chose "pure Rust, spike first", 2026-11-14T07:42Z)

## Context

Spec 044 adds a phone app that plays music and audiobooks in Android Auto,
with voice requests through Gemini. Android Auto never shows an app's own UI:
it binds the app's media browser service and draws its own lists and
now-playing screen. Android starts every app component (service, activity,
content provider) by instantiating a Java class named in the manifest, from
the app's own dex. Native code cannot be the entry point for a service:
the platform ships `android.app.NativeActivity` for activities only.

Pharos client code is pure Rust by decision (spec 025, 2026-11-04: "no Kotlin,
pure Rust"). The usual Rust–JVM tools (robusta, jnix, jni-high, jni-min-helper)
all expect hand-written Java for any class the platform instantiates; a dex
loaded at runtime (`InMemoryDexClassLoader`) cannot be named in the manifest,
because the platform loads the component before app code runs.
`java.lang.reflect.Proxy` implements interfaces only, while
`MediaBrowserService`, `MediaSession.Callback`, `ContentProvider` and
`Activity` are classes.

## Decision

We will generate each Java class the platform must instantiate as JVM
class-file bytes from Rust at build time (`spikes/044-android-auto/classgen`,
no dependencies), dex it with the SDK's `d8`, and name it in the manifest.
Every generated method is one of four fixed shapes: a superclass-calling
constructor, a `System.loadLibrary` static initialiser, a `native` method,
or a void "super, then private native hook" method for lifecycle callbacks
whose framework implementation must still run (`onCreate`, `onDestroy`).
All behaviour lives in Rust behind JNI (`jni` crate). Class version 52 and
branch-free code keep the generator free of stack-map frames.

We use the framework media classes (`android.service.media.MediaBrowserService`,
`android.media.session.MediaSession`) rather than AndroidX/Media3, so the APK
needs no Java libraries at all. Interfaces (listeners) are implemented at
runtime with `java.lang.reflect.Proxy` or, where a class is required, one
more generated stub.

## Consequences

- No Java or Kotlin source exists in the repo; the stubs are build output,
  like the launcher `dx` generates for the TV app.
- Proven on a Pixel 9 Pro (Android 16, Android Auto 18.0) in the Desktop Head
  Unit on 2026-11-14: Android Auto listed the app, browsed it, played audio,
  and play/pause/next/previous/stop reached Rust; ART raised no
  `VerifyError` or `UnsatisfiedLinkError`.
- JNI calls are more verbose than Kotlin; every framework call is a string
  signature checked only at run time. `jvm-check/` runs the generated classes
  and the Rust library on a desktop JVM against stand-in `android.*` classes
  to catch signature mistakes before a device does, and goes red when the
  library is missing or the bytecode is broken.
- Framework `MediaBrowserService` lacks Media3 conveniences (automatic
  notification, audio focus, queue helpers); Rust must do these itself.
- A new framework base class means a new `ClassSpec`; a method shape outside
  the four (e.g. a non-void super call) means extending the generator.

## Alternatives considered

- **A small hand-written Kotlin service on Media3 (~1k lines).** Fastest
  (~3–4 weeks to music in the car) but breaks the pure-Rust rule.
- **A ~150-line hand-written Java forwarding stub.** Near-pure; still Java
  source to maintain. The generator replaces exactly this.
- **Runtime-generated or dynamically loaded classes.** Cannot be manifest
  components.
- **Phone app without Android Auto.** Rejected by the goal.

## References

- `specs/044-android-auto/spec.md`, `specs/044-android-auto/spike-0.md`
- ADR-0061 / spec 025 (pure-Rust Android TV app)
- https://developer.android.com/training/cars/media
- https://developer.android.com/training/cars/testing/dhu
