import android.media.MediaPlayer;
import android.media.browse.MediaBrowser;
import android.media.session.MediaSession;
import android.service.media.MediaBrowserService;
import com.alisonjenkins.pharos.auto.PharosMediaService;
import java.util.List;

/** Drives the generated classes on a desktop JVM against stand-in android.* classes. */
public class Check {
  static int failures = 0;
  static void expect(boolean ok, String what) { System.out.println((ok ? "PASS " : "FAIL ") + what); if (!ok) failures++; }

  public static void main(String[] a) throws Exception {
    PharosMediaService s = new PharosMediaService();
    s.onCreate();
    expect(s.superCreated, "onCreate ran the framework's onCreate first");
    expect(s.token != null, "Rust set the session token");
    MediaBrowserService.BrowserRoot root = s.onGetRoot("com.google.android.projection.gearhead", 10123, null);
    expect(root != null && root.getRootId().equals("root"), "onGetRoot returned root");
    expect(root.extras != null && Boolean.TRUE.equals(root.extras.m.get("android.media.browse.SEARCH_SUPPORTED")), "root says search is supported");
    MediaBrowserService.Result<List<MediaBrowser.MediaItem>> r = new MediaBrowserService.Result<>();
    s.onLoadChildren("root", r);
    expect(r.sent && r.value.size() == 2, "root has Music and Audiobooks: " + r.value);
    r = new MediaBrowserService.Result<>();
    s.onLoadChildren("music", r);
    expect(r.sent && r.value.size() == 3 && r.value.get(0).flags == 2, "music has two tones and a stream: " + r.value);
    android.net.Uri icon = r.value.get(2).d.icon;
    expect(icon != null && icon.toString().startsWith("content://com.alisonjenkins.pharos.auto.art/"), "the stream has a content:// icon: " + icon);

    // Typed as the framework class, so a descriptor typo fails here.
    android.content.ContentProvider art = new com.alisonjenkins.pharos.auto.PharosArtProvider();
    expect(art.onCreate(), "art provider onCreate");
    expect("image/*".equals(art.getType(icon)), "art type is image/*");
    expect(art.query(icon, null, null, null, null) == null && art.insert(icon, null) == null
        && art.delete(icon, null, null) == 0 && art.update(icon, null, null, null) == 0, "art provider is read-only");
    java.io.File cached = new java.io.File(new android.content.Context().getCacheDir(), "art/" + icon.getLastPathSegment());
    cached.getParentFile().mkdirs();
    java.nio.file.Files.write(cached.toPath(), new byte[] {1, 2, 3});
    android.os.ParcelFileDescriptor pfd = art.openFile(icon, "r");
    expect(pfd.file.equals(cached) && pfd.mode == android.os.ParcelFileDescriptor.MODE_READ_ONLY, "openFile serves the cached image read-only");
    cached.delete();
    for (String bad : new String[] {"content://com.alisonjenkins.pharos.auto.art/00000000deadbeef", "content://com.alisonjenkins.pharos.auto.art/..%2f..%2fx"}) {
      boolean refused = false;
      try { art.openFile(android.net.Uri.parse(bad), "r"); } catch (java.io.FileNotFoundException e) { refused = true; }
      expect(refused, "openFile refuses " + bad);
    }
    boolean writeRefused = false;
    try { art.openFile(icon, "w"); } catch (java.io.FileNotFoundException e) { writeRefused = true; }
    expect(writeRefused, "openFile refuses write mode");

    r = new MediaBrowserService.Result<>();
    s.onSearch("chapter", null, r);
    expect(r.sent && r.value.size() == 1, "voice search finds the chapter: " + r.value);

    // Rust state is global, so a fresh callback drives the same session.
    MediaSession.Callback cb = new com.alisonjenkins.pharos.auto.PharosSessionCallback();
    cb.onPlayFromMediaId("tone:440", null);
    MediaSession session = MediaSession.last;
    expect(session.state.state == 6, "a new item is BUFFERING until prepared: " + session.state);
    expect(session.flags == 3, "session handles media buttons and transport controls");
    expect(session.activity != null && session.activity.intent.target.getSimpleName().equals("PharosSearchActivity")
        && session.activity.flags == 67108864, "session activity is an immutable intent for PharosSearchActivity");
    expect((session.state.actions & (16384 | 32768 | 65536)) == (16384 | 32768 | 65536), "PREPARE actions advertised");
    cb.onPause();
    expect(session.state.state == 6, "pause while preparing is ignored: " + session.state);
    MediaPlayer.last.firePrepared();
    expect(session.state.state == 3, "onPrepared starts playback: " + session.state);
    cb.onPause();
    cb.onPlay();
    cb.onSeekTo(4000);
    cb.onSkipToNext();
    cb.onPlayFromSearch("test tone c", null);

    cb.onPlayFromMediaId("stream:example", null);
    MediaPlayer stream = MediaPlayer.last;
    expect(stream.source().startsWith("https://"), "a stream entry plays its URL: " + stream.source());
    expect(String.valueOf(session.md.m.get("android.media.metadata.ALBUM_ART_URI")).equals(icon.toString())
        && icon.toString().equals(session.md.m.get("android.media.metadata.DISPLAY_ICON_URI")), "stream metadata carries its album art: " + session.md);
    cb.onPlayFromMediaId("tone:523", null);
    MediaPlayer tone = MediaPlayer.last;
    stream.firePrepared();
    expect(session.state.state == 6, "a replaced player's late onPrepared does not start it: " + session.state);
    expect(tone.fireError(1, -1004), "onError reports the error handled");
    expect(session.state.state == 7 && tone.isReleased(), "onError sets ERROR and releases: " + session.state);
    cb.onPlayFromMediaId("tone:440", null);
    MediaPlayer.last.firePrepared();
    MediaPlayer.last.fireCompletion();
    expect("tone:523".equals(session.md.m.get("android.media.metadata.MEDIA_ID")), "completion skips to the next item: " + session.md);
    android.os.Bundle hints = new android.os.Bundle();
    hints.putString("android.intent.extra.focus", "vnd.android.cursor.item/audio");
    cb.onPrepareFromSearch("test chapter", hints);
    expect(session.state.state == 2 && "tone:330".equals(session.md.m.get("android.media.metadata.MEDIA_ID")),
        "onPrepareFromSearch loads the match paused: " + session.state);
    cb.onPlayFromSearch("test tone a", hints);
    cb.onPlayFromMediaId("nope", null);
    cb.onStop();
    s.onDestroy();
    System.out.println(failures == 0 ? "ALL PASS" : failures + " FAILED");
    System.exit(failures == 0 ? 0 : 1);
  }
}
