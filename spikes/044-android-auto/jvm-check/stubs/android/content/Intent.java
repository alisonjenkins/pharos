package android.content;
public class Intent {
  public final Class<?> target;
  public final String action;
  private final java.util.Map<String, Object> extras = new java.util.HashMap<>();
  public Intent(Context c, Class<?> cls) { target = cls; action = null; }
  public Intent(String action) { target = null; this.action = action; }
  public Intent putExtra(String k, boolean v) { extras.put(k, v); return this; }
  public Intent putExtra(String k, long v) { extras.put(k, v); return this; }
  public boolean hasExtra(String k) { return extras.containsKey(k); }
  public boolean getBooleanExtra(String k, boolean d) { Object v = extras.get(k); return v instanceof Boolean ? (Boolean) v : d; }
  public long getLongExtra(String k, long d) { Object v = extras.get(k); return v instanceof Long ? (Long) v : d; }
}
