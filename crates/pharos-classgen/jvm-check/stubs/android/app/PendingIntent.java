package android.app;
public final class PendingIntent {
  public final android.content.Intent intent; public final int flags;
  PendingIntent(android.content.Intent i, int f) { intent = i; flags = f; }
  public static PendingIntent getActivity(android.content.Context c, int code, android.content.Intent i, int flags) { return new PendingIntent(i, flags); }
}
