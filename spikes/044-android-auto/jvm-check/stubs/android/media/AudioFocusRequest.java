package android.media;
public final class AudioFocusRequest {
  public int gain; public AudioAttributes attrs; public AudioManager.OnAudioFocusChangeListener listener;
  public static class Builder {
    final AudioFocusRequest r = new AudioFocusRequest();
    public Builder(int gain) { r.gain = gain; }
    public Builder setAudioAttributes(AudioAttributes a) { r.attrs = a; return this; }
    public Builder setOnAudioFocusChangeListener(AudioManager.OnAudioFocusChangeListener l) { r.listener = l; return this; }
    public AudioFocusRequest build() { return r; }
  }
}
