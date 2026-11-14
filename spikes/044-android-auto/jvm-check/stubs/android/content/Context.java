package android.content;
public class Context {
  public java.io.File getCacheDir() { return new java.io.File(System.getProperty("java.io.tmpdir")); }
  public java.io.File getFilesDir() { java.io.File d = new java.io.File(getCacheDir(), "pharos-jvm-check-files"); d.mkdirs(); return d; }
}
