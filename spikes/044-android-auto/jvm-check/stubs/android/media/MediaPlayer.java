package android.media;
public class MediaPlayer {
  String src; int pos;
  public MediaPlayer() {}
  public void setDataSource(String path) { src = path; System.out.println("stub: MediaPlayer.setDataSource " + path + " exists=" + new java.io.File(path).length()); }
  public void prepare() { System.out.println("stub: MediaPlayer.prepare"); }
  public void start() { System.out.println("stub: MediaPlayer.start"); }
  public void pause() { System.out.println("stub: MediaPlayer.pause"); }
  public void stop() { System.out.println("stub: MediaPlayer.stop"); }
  public void release() { System.out.println("stub: MediaPlayer.release"); }
  public void seekTo(int ms) { pos = ms; System.out.println("stub: MediaPlayer.seekTo " + ms); }
  public int getCurrentPosition() { return pos; }
}
