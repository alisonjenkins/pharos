package android.content;
public class Context {
  public java.io.File getCacheDir() { return new java.io.File(System.getProperty("java.io.tmpdir")); }
  public Object getSystemService(String name) { if ("connectivity".equals(name)) return android.net.ConnectivityManager.last != null ? android.net.ConnectivityManager.last : new android.net.ConnectivityManager(); return "audio".equals(name) ? (android.media.AudioManager.last != null ? android.media.AudioManager.last : new android.media.AudioManager()) : null; }
  public static BroadcastReceiver receiver;
  public static IntentFilter filter;
  public static int receiverFlags;
  public static boolean receiverUnregistered;
  public Intent registerReceiver(BroadcastReceiver r, IntentFilter f, int flags) { receiver = r; filter = f; receiverFlags = flags; receiverUnregistered = false; return null; }
  public void unregisterReceiver(BroadcastReceiver r) { if (r == receiver) receiverUnregistered = true; }
  public java.io.File getFilesDir() { java.io.File d = new java.io.File(getCacheDir(), "pharos-jvm-check-files"); d.mkdirs(); return d; }
}
