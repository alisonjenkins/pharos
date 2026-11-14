package android.media;
public final class AudioAttributes {
  public int usage, contentType;
  public String toString() { return "AudioAttributes(usage=" + usage + " content=" + contentType + ")"; }
  public static class Builder {
    final AudioAttributes a = new AudioAttributes();
    public Builder() {}
    public Builder setUsage(int u) { a.usage = u; return this; }
    public Builder setContentType(int c) { a.contentType = c; return this; }
    public AudioAttributes build() { return a; }
  }
}
