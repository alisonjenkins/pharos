package android.media.browse;
import android.media.MediaDescription;
public final class MediaBrowser {
  public static class MediaItem {
    public static final int FLAG_BROWSABLE = 1, FLAG_PLAYABLE = 2;
    public final MediaDescription d; public final int flags;
    public MediaItem(MediaDescription description, int flags) { d = description; this.flags = flags; }
    public String toString() { return d.getMediaId() + "|" + d.getTitle() + "|flags=" + flags; }
  }
}
