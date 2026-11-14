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
    r = new MediaBrowserService.Result<>();
    s.onSearch("chapter", null, r);
    expect(r.sent && r.value.size() == 1, "voice search finds the chapter: " + r.value);

    // Rust state is global, so a fresh callback drives the same session.
    MediaSession.Callback cb = new com.alisonjenkins.pharos.auto.PharosSessionCallback();
    cb.onPlayFromMediaId("tone:440", null);
    MediaSession session = MediaSession.last;
    expect(session.state.state == 6, "a new item is BUFFERING until prepared: " + session.state);
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
    cb.onPlayFromMediaId("nope", null);
    cb.onStop();
    s.onDestroy();
    System.out.println(failures == 0 ? "ALL PASS" : failures + " FAILED");
    System.exit(failures == 0 ? 0 : 1);
  }
}
