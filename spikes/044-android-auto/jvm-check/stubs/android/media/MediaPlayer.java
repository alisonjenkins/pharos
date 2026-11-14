package android.media;
/** Records listeners; the test fires them, as the real player does later on its looper. */
public class MediaPlayer {
  public static MediaPlayer last;
  String src; int pos; boolean released;
  OnPreparedListener prepared; OnErrorListener error; OnCompletionListener completion;
  public interface OnPreparedListener { void onPrepared(MediaPlayer mp); }
  public interface OnErrorListener { boolean onError(MediaPlayer mp, int what, int extra); }
  public interface OnCompletionListener { void onCompletion(MediaPlayer mp); }
  public MediaPlayer() { last = this; }
  public void setOnPreparedListener(OnPreparedListener l) { prepared = l; }
  public void setOnErrorListener(OnErrorListener l) { error = l; }
  public void setOnCompletionListener(OnCompletionListener l) { completion = l; }
  public void setDataSource(String path) { src = path; System.out.println("stub: MediaPlayer.setDataSource " + path); }
  public void prepareAsync() { System.out.println("stub: MediaPlayer.prepareAsync"); }
  public void start() { System.out.println("stub: MediaPlayer.start"); }
  public void pause() { System.out.println("stub: MediaPlayer.pause"); }
  public void stop() { System.out.println("stub: MediaPlayer.stop"); }
  public void release() { released = true; System.out.println("stub: MediaPlayer.release"); }
  public void seekTo(int ms) { pos = ms; System.out.println("stub: MediaPlayer.seekTo " + ms); }
  public int getCurrentPosition() { return pos; }
  public int getDuration() { return 10000; }
  public String source() { return src; }
  public boolean isReleased() { return released; }
  public void firePrepared() { prepared.onPrepared(this); }
  public boolean fireError(int what, int extra) { return error.onError(this, what, extra); }
  public void fireCompletion() { completion.onCompletion(this); }
}
