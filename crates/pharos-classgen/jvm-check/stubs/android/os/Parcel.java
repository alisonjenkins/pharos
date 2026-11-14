package android.os;
/** A list of values; each takes 4 "bytes" of position so lengths add up as on a device. */
public final class Parcel {
  final java.util.ArrayList<Object> v = new java.util.ArrayList<>();
  int pos;
  public boolean recycled;
  public static Parcel obtain() { return new Parcel(); }
  void put(Object o) { int i = pos / 4; if (i < v.size()) v.set(i, o); else v.add(o); pos += 4; }
  Object take() { Object o = v.get(pos / 4); pos += 4; return o; }
  public void writeInt(int i) { put(i); }
  public void writeString(String s) { put(s); }
  public void writeStrongBinder(IBinder b) { put(b); }
  public void writeInterfaceToken(String s) { put(s); }
  public void writeRaw(Object o) { put(o); }
  public int readInt() { return (Integer) take(); }
  public String readString() { return (String) take(); }
  public IBinder readStrongBinder() { return (IBinder) take(); }
  public Object readRaw() { return take(); }
  public int dataPosition() { return pos; }
  public void setDataPosition(int p) { pos = p; }
  public void recycle() { recycled = true; }
}
