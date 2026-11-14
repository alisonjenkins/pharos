package android.os;
public final class Message {
  public int what; Bundle data;
  public Message() {}
  public Bundle getData() { if (data == null) data = new Bundle(); return data; }
  public void setData(Bundle b) { data = b; }
}
