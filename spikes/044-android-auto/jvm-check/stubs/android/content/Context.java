package android.content;
public class Context {
  public java.io.File getCacheDir() { return new java.io.File(System.getProperty("java.io.tmpdir")); }
  public Object getSystemService(String name) { if ("connectivity".equals(name)) return android.net.ConnectivityManager.last != null ? android.net.ConnectivityManager.last : new android.net.ConnectivityManager(); return "audio".equals(name) ? (android.media.AudioManager.last != null ? android.media.AudioManager.last : new android.media.AudioManager()) : null; }
  public java.io.File getFilesDir() { java.io.File d = new java.io.File(getCacheDir(), "pharos-jvm-check-files"); d.mkdirs(); return d; }
}
