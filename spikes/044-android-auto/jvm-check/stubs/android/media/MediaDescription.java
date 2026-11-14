package android.media;
public class MediaDescription {
  public String id; public CharSequence title, subtitle;
  public String getMediaId() { return id; } public CharSequence getTitle() { return title; }
  public static class Builder {
    MediaDescription d = new MediaDescription();
    public Builder() {}
    public Builder setMediaId(String s) { d.id = s; return this; }
    public Builder setTitle(CharSequence s) { d.title = s; return this; }
    public Builder setSubtitle(CharSequence s) { d.subtitle = s; return this; }
    public MediaDescription build() { return d; }
  }
}
