package android.service.media;
import android.os.Bundle;
import android.media.session.MediaSession;
import android.media.browse.MediaBrowser;
import java.util.List;
public abstract class MediaBrowserService extends android.app.Service {
  public boolean superCreated;
  public MediaSession.Token token;
  @Override public void onCreate() { superCreated = true; System.out.println("stub: MediaBrowserService.onCreate (super ran)"); }
  public abstract BrowserRoot onGetRoot(String clientPackageName, int clientUid, Bundle rootHints);
  public abstract void onLoadChildren(String parentId, Result<List<MediaBrowser.MediaItem>> result);
  public void onSearch(String query, Bundle extras, Result<List<MediaBrowser.MediaItem>> result) { result.sendResult(null); }
  public void setSessionToken(MediaSession.Token token) { this.token = token; }
  public static final class BrowserRoot {
    public final String id;
    public final Bundle extras;
    public BrowserRoot(String rootId, Bundle extras) { id = rootId; this.extras = extras; }
    public String getRootId() { return id; }
  }
  public static class Result<T> {
    public T value; public boolean sent, detached;
    public Result() {}
    public void detach() { detached = true; }
    public synchronized void sendResult(T result) { value = result; sent = true; notifyAll(); }
    /** Waits for a result sent from another thread, as the framework would. */
    public synchronized Result<T> await() throws InterruptedException {
      long end = System.currentTimeMillis() + 30_000;
      while (!sent && System.currentTimeMillis() < end) wait(end - System.currentTimeMillis());
      return this;
    }
  }
}
