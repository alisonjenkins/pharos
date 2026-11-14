package android.media;
public final class MediaMetadata {
  public final java.util.Map<String,Object> m = new java.util.TreeMap<>();
  public String toString() { return m.toString(); }
  public static final class Builder {
    MediaMetadata md = new MediaMetadata();
    public Builder() {}
    public Builder putString(String k, String v) { md.m.put(k, v); return this; }
    public Builder putLong(String k, long v) { md.m.put(k, v); return this; }
    public MediaMetadata build() { return md; }
  }
}
