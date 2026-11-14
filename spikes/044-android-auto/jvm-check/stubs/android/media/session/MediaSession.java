package android.media.session;
import android.os.Bundle;
public final class MediaSession {
  public Callback cb; public boolean active; public PlaybackState state; public android.media.MediaMetadata md;
  public static MediaSession last;
  public MediaSession(android.content.Context context, String tag) { last = this; System.out.println("stub: MediaSession(" + tag + ")"); }
  public void setCallback(Callback callback) { cb = callback; }
  public void setActive(boolean a) { active = a; }
  public Token getSessionToken() { return new Token(); }
  public void setPlaybackState(PlaybackState s) { state = s; System.out.println("stub: setPlaybackState " + s); }
  public void setMetadata(android.media.MediaMetadata m) { md = m; System.out.println("stub: setMetadata " + m); }
  public void release() { System.out.println("stub: MediaSession.release"); }
  public static final class Token {}
  public abstract static class Callback {
    public Callback() {}
    public void onPlay() {} public void onPause() {} public void onStop() {}
    public void onSkipToNext() {} public void onSkipToPrevious() {} public void onSeekTo(long pos) {}
    public void onPlayFromMediaId(String id, Bundle extras) {} public void onPlayFromSearch(String q, Bundle extras) {}
  }
}
