package android.app;
public class NotificationManager {
  public static NotificationManager last;
  public NotificationChannel channel;
  /** Posted notifications by id; cancel removes. */
  public final java.util.Map<Integer, Notification> posted = new java.util.HashMap<>();
  public NotificationManager() { last = this; }
  public void createNotificationChannel(NotificationChannel c) { channel = c; }
  public void notify(int id, Notification n) { posted.put(id, n); }
  public void cancel(int id) { posted.remove(id); }
}
