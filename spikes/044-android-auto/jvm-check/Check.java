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

  /** Records the one transaction the app sends to a search's result receiver. */
  static final class Receiver implements android.os.IBinder {
    android.os.Parcel data; int code, flags;
    public synchronized boolean transact(int code, android.os.Parcel data, android.os.Parcel reply, int flags) {
      this.code = code; this.data = data; this.flags = flags; notifyAll(); return true;
    }
    synchronized android.os.Parcel await() throws InterruptedException {
      long end = System.currentTimeMillis() + 30_000;
      while (data == null && System.currentTimeMillis() < end) wait(end - System.currentTimeMillis());
      return data;
    }
  }
  /** Stands in for androidx's ResultReceiver: its parcel is just its binder. */
  static final class ReceiverParcelable implements android.os.Parcelable {
    final Receiver r; ReceiverParcelable(Receiver r) { this.r = r; }
    public void writeToParcel(android.os.Parcel p, int f) { p.writeStrongBinder(r); }
  }

  /** Android Auto's search: MSG_SEARCH to the Messenger in the root extras. */
  static void compatSearch(android.os.Bundle extras) throws InterruptedException {
    Object binder = extras.m.get("extra_messenger");
    expect(binder instanceof android.os.Messenger.Binder, "root extras carry a compat Messenger binder: " + binder);
    expect(Integer.valueOf(1).equals(extras.m.get("extra_service_version")), "compat service version 1, so browsing stays on the framework");
    android.os.Message msg = new android.os.Message();
    msg.what = 8;
    Receiver recv = new Receiver();
    android.os.Bundle searchExtras = new android.os.Bundle();
    searchExtras.putString("android.intent.extra.focus", "vnd.android.cursor.item/audio");
    msg.getData().putString("data_search_query", "test tone c");
    msg.getData().putBundle("data_search_extras", searchExtras);
    msg.getData().putParcelable("data_result_receiver", new ReceiverParcelable(recv));
    msg.getData().putInt("data_calling_pid", 1234);
    ((android.os.Messenger.Binder) binder).messenger.handler.handleMessage(msg);
    android.os.Parcel p = recv.await();
    expect(p != null && recv.code == 1 && recv.flags == 1, "search replies with a oneway send() to the receiver");
    if (p == null) return;
    p.setDataPosition(0);
    expect("android.support.v4.os.IResultReceiver".equals(p.readString()) && p.readInt() == 0 && p.readInt() == 1,
        "reply is IResultReceiver.send(RESULT_OK, bundle)");
    p.readInt(); p.readInt();
    expect(p.readInt() == 1 && "search_results".equals(p.readString()) && p.readInt() == 16, "reply bundle holds search_results");
    p.readInt();
    Object[] items = (Object[]) p.readRaw();
    expect(items.length == 1 && items[0].getClass().getName().equals("android.support.v4.media.MediaBrowserCompat$MediaItem"),
        "results parcel under androidx's MediaItem name: " + java.util.Arrays.toString(items));
    MediaBrowser.MediaItem hit = (MediaBrowser.MediaItem) items[0];
    expect(hit.flags == 2 && "tone:523".equals(hit.d.getMediaId()), "compat search finds tone C: " + hit);
    android.os.Message other = new android.os.Message();
    other.what = 6;
    ((android.os.Messenger.Binder) binder).messenger.handler.handleMessage(other);
  }

  public static void main(String[] a) throws Exception {
    // A closed port: library calls fail fast, and the Test folder must still work.
    java.nio.file.Files.write(new java.io.File(new android.content.Context().getFilesDir(), "server_url").toPath(),
        "http://127.0.0.1:9\n".getBytes());
    new java.io.File(new android.content.Context().getFilesDir(), "device_token").delete();
    PharosMediaService s = new PharosMediaService();
    s.onCreate();
    android.net.ConnectivityManager cm = android.net.ConnectivityManager.last;
    expect(cm != null && cm.callback != null && cm.callback.getClass().getSimpleName().equals("PharosNetworkCallback"),
        "service registers a default-network callback");
    if (cm != null && cm.callback != null) { cm.callback.onLost(new android.net.Network()); cm.callback.onAvailable(new android.net.Network()); }
    expect(s.superCreated, "onCreate ran the framework's onCreate first");
    expect(s.token != null, "Rust set the session token");
    MediaBrowserService.BrowserRoot root = s.onGetRoot("com.google.android.projection.gearhead", 10123, null);
    expect(root != null && root.getRootId().equals("root"), "onGetRoot returned root");
    expect(root.extras != null && Boolean.TRUE.equals(root.extras.m.get("android.media.browse.SEARCH_SUPPORTED")), "root says search is supported");
    compatSearch(root.extras);
    MediaBrowserService.Result<List<MediaBrowser.MediaItem>> r = new MediaBrowserService.Result<>();
    s.onLoadChildren("root", r);
    expect(r.detached, "root is answered off the main thread");
    r.await();
    expect(r.sent && r.value.size() == 2 && r.value.get(0).flags == 0 && "test".equals(r.value.get(1).d.getMediaId())
        && r.value.get(1).flags == 1, "offline root is a notice row and the Test folder: " + r.value);
    r = new MediaBrowserService.Result<>();
    s.onLoadChildren("test", r);
    r.await();
    expect(r.sent && r.value.size() == 4 && r.value.get(0).flags == 2, "Test has three tones and a stream: " + r.value);
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
    r.await();
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
    android.media.AudioManager am = android.media.AudioManager.last;
    expect(session.local != null && session.local.usage == 1 && session.local.contentType == 2, "session plays locally with media attributes: " + session.local);
    expect(MediaPlayer.last.attrsBeforeSource && MediaPlayer.last.attrs.usage == 1, "the player has media attributes before its source");
    expect(am != null && am.holder != null && am.holder.gain == 1 && am.holder.attrs == session.local, "starting holds media audio focus");
    am.fireChange(-3);
    expect(MediaPlayer.last.volume < 1 && MediaPlayer.last.playing, "a can-duck loss lowers the volume and keeps playing");
    am.fireChange(1);
    expect(MediaPlayer.last.volume == 1, "regaining focus restores the volume");
    am.fireChange(-2);
    expect(session.state.state == 2 && !MediaPlayer.last.playing, "a transient loss pauses: " + session.state);
    am.fireChange(1);
    expect(session.state.state == 3 && MediaPlayer.last.playing, "regaining focus after a transient loss resumes: " + session.state);
    cb.onPause();
    am.grant = 0;
    cb.onPlay();
    expect(session.state.state == 2 && !MediaPlayer.last.playing, "refused focus does not start playback: " + session.state);
    am.grant = 1;
    am.fireChange(-1);
    expect(am.holder == null, "a permanent loss gives focus back");
    cb.onPause();
    cb.onSeekTo(4000);
    expect(session.state.state == 2 && session.state.pos == 4000 && session.state.speed == 0, "a seek while paused stays paused at the new position: " + session.state);
    cb.onPlay();
    expect(session.state.state == 3 && session.state.pos == 4000 && session.state.speed == 1, "play resumes from the seek position: " + session.state);
    cb.onSeekTo(6000);
    expect(session.state.state == 3 && session.state.pos == 6000, "a seek while playing keeps playing: " + session.state);
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
    MediaPlayer.last.firePrepared();
    cb.onPlayFromSearch("Next song", null);
    expect("stream:example".equals(session.md.m.get("android.media.metadata.MEDIA_ID")), "a spoken \"next song\" skips instead of searching: " + session.md);
    android.os.Bundle hints = new android.os.Bundle();
    hints.putString("android.intent.extra.focus", "vnd.android.cursor.item/audio");
    cb.onPrepareFromSearch("test chapter", hints);
    expect(session.state.state == 2 && "tone:330".equals(session.md.m.get("android.media.metadata.MEDIA_ID")),
        "onPrepareFromSearch loads the match paused: " + session.state);
    cb.onPlayFromSearch("qqq nothing like this", hints);
    for (int i = 0; i < 300 && session.state.state != 7; i++) Thread.sleep(100);
    expect(session.state.state == 7 && "Search failed".equals(String.valueOf(session.state.error)),
        "a voice request the library cannot answer is an error the car can say, not a fallback: " + session.state);
    cb.onPlayFromSearch("test tone a", hints);
    cb.onPlayFromMediaId("nope", null);
    cb.onStop();
    s.onDestroy();
    expect(cm != null && cm.callback == null && cm.unregisters == 1, "onDestroy unregisters the network callback");
    System.out.println(failures == 0 ? "ALL PASS" : failures + " FAILED");
    System.exit(failures == 0 ? 0 : 1);
  }
}
