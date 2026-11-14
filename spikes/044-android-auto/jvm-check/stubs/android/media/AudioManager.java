package android.media;
/** Grants every request and records the holder; the test fires focus changes. */
public class AudioManager {
  public static AudioManager last;
  public AudioFocusRequest holder; public int requests, abandons; public int grant = 1;
  public AudioManager() { last = this; }
  public interface OnAudioFocusChangeListener { void onAudioFocusChange(int change); }
  public int requestAudioFocus(AudioFocusRequest r) { requests++; if (grant == 1) holder = r; System.out.println("stub: requestAudioFocus -> " + grant); return grant; }
  public int abandonAudioFocusRequest(AudioFocusRequest r) { abandons++; if (holder == r) holder = null; return 1; }
  public void fireChange(int change) { holder.listener.onAudioFocusChange(change); }
}
