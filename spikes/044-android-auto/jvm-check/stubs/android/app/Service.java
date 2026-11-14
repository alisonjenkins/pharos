package android.app;
public abstract class Service extends android.content.Context {
  /** Foreground state the code under test asked for. */
  public android.content.Intent startedService;
  public Notification foregroundNotification;
  public int foregroundId, foregroundType, stopForegroundFlags = -1, stopSelfCalls;
  public boolean inForeground;
  public void onCreate() { System.out.println("stub: Service.onCreate"); }
  public void onDestroy() { System.out.println("stub: Service.onDestroy"); }
  public final void startForeground(int id, Notification n, int type) { foregroundId = id; foregroundNotification = n; foregroundType = type; inForeground = true; }
  public final void stopForeground(int flags) { stopForegroundFlags = flags; inForeground = false; }
  public final void stopSelf() { stopSelfCalls++; }
}
