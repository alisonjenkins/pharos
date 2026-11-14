package android.os;
public final class Bundle {
  public final java.util.Map<String,Object> m = new java.util.LinkedHashMap<>();
  public Bundle() {}
  public void putBoolean(String k, boolean v) { m.put(k, v); }
  public void putString(String k, String v) { m.put(k, v); }
  public void putInt(String k, int v) { m.put(k, v); }
  public void putBundle(String k, Bundle v) { m.put(k, v); }
  public void putBinder(String k, IBinder v) { m.put(k, v); }
  public void putParcelable(String k, Parcelable v) { m.put(k, v); }
  public void putParcelableArray(String k, Parcelable[] v) { m.put(k, v); }
  public java.util.Set<String> keySet() { return m.keySet(); }
  /** The API 33+ layout the app reads: length, magic, count, then key, tag, value. */
  public void writeToParcel(Parcel p, int flags) {
    if (m.isEmpty()) { p.writeInt(0); return; }
    int lengthAt = p.dataPosition();
    p.writeInt(-1);
    p.writeInt(0x4C444E42);
    int start = p.dataPosition();
    p.writeInt(m.size());
    for (java.util.Map.Entry<String,Object> e : m.entrySet()) {
      p.writeString(e.getKey());
      Object v = e.getValue();
      if (v == null) { p.writeInt(-1); }
      else if (v instanceof String) { p.writeInt(0); p.writeString((String) v); }
      else if (v instanceof Integer) { p.writeInt(1); p.writeInt((Integer) v); }
      else if (v instanceof Bundle) { p.writeInt(3); ((Bundle) v).writeToParcel(p, 0); }
      else if (v instanceof Parcelable) { p.writeInt(4); prefixed(p, () -> { p.writeString(v.getClass().getName()); ((Parcelable) v).writeToParcel(p, 0); }); }
      else if (v instanceof Parcelable[]) { p.writeInt(16); prefixed(p, () -> p.writeRaw(v)); }
      else throw new IllegalArgumentException("stub Bundle cannot parcel " + v);
    }
    int end = p.dataPosition();
    p.setDataPosition(lengthAt); p.writeInt(end - start); p.setDataPosition(end);
  }
  private static void prefixed(Parcel p, Runnable body) {
    int at = p.dataPosition(); p.writeInt(-1);
    int start = p.dataPosition(); body.run(); int end = p.dataPosition();
    p.setDataPosition(at); p.writeInt(end - start); p.setDataPosition(end);
  }
}
