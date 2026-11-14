package android.app;
public class Notification {
  public String channel; public CharSequence title, text; public int icon, visibility; public Style style;
  public abstract static class Style {}
  public static class MediaStyle extends Style {
    public android.media.session.MediaSession.Token token;
    public MediaStyle setMediaSession(android.media.session.MediaSession.Token t) { token = t; return this; }
  }
  public static class Builder {
    private final Notification n = new Notification();
    public Builder(android.content.Context c, String channel) { n.channel = channel; }
    public Builder setSmallIcon(int i) { n.icon = i; return this; }
    public Builder setContentTitle(CharSequence t) { n.title = t; return this; }
    public Builder setContentText(CharSequence t) { n.text = t; return this; }
    public Builder setVisibility(int v) { n.visibility = v; return this; }
    public Builder setStyle(Style s) { n.style = s; return this; }
    public Notification build() { return n; }
  }
}
