package android.media.session;
public final class PlaybackState {
  public int state; public long pos, actions; public float speed;
  public String toString() { return "state=" + state + " pos=" + pos + " speed=" + speed + " actions=" + actions; }
  public static final class Builder {
    PlaybackState p = new PlaybackState();
    public Builder() {}
    public Builder setState(int state, long position, float playbackSpeed) { p.state = state; p.pos = position; p.speed = playbackSpeed; return this; }
    public Builder setActions(long a) { p.actions = a; return this; }
    public PlaybackState build() { return p; }
  }
}
