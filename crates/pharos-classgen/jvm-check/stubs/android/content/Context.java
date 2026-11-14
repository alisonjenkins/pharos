package android.content;
public class Context {
  public java.io.File getCacheDir() { return new java.io.File(System.getProperty("java.io.tmpdir")); }
  public ComponentName startForegroundService(Intent i) { if (this instanceof android.app.Service) ((android.app.Service) this).startedService = i; return null; }
  public Object getSystemService(String name) { if ("notification".equals(name)) return android.app.NotificationManager.last != null ? android.app.NotificationManager.last : new android.app.NotificationManager(); if ("connectivity".equals(name)) return android.net.ConnectivityManager.last != null ? android.net.ConnectivityManager.last : new android.net.ConnectivityManager(); return "audio".equals(name) ? (android.media.AudioManager.last != null ? android.media.AudioManager.last : new android.media.AudioManager()) : null; }
  /** Registered receivers by filter action; flags by action; receivers unregistered since. */
  public static final java.util.Map<String, BroadcastReceiver> receivers = new java.util.HashMap<>();
  public static final java.util.Map<String, Integer> receiverFlags = new java.util.HashMap<>();
  public static final java.util.Set<BroadcastReceiver> unregistered = new java.util.HashSet<>();
  public Intent registerReceiver(BroadcastReceiver r, IntentFilter f, int flags) { receivers.put(f.action, r); receiverFlags.put(f.action, flags); unregistered.remove(r); return null; }
  public void unregisterReceiver(BroadcastReceiver r) { if (receivers.containsValue(r)) unregistered.add(r); }
  public java.io.File getFilesDir() { java.io.File d = new java.io.File(getCacheDir(), "pharos-jvm-check-files"); d.mkdirs(); return d; }
}
