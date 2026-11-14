package android.os;
/** Its binder delivers a message straight to the handler, as the real one does on its looper. */
public final class Messenger {
  public final Handler handler;
  public Messenger(Handler h) { handler = h; }
  public IBinder getBinder() { return new Binder(this); }
  public static final class Binder implements IBinder {
    public final Messenger messenger;
    Binder(Messenger m) { messenger = m; }
    public boolean transact(int code, Parcel data, Parcel reply, int flags) { return false; }
  }
}
