package android.os;
public final class ParcelFileDescriptor {
  public static final int MODE_READ_ONLY = 0x10000000;
  public final java.io.File file; public final int mode;
  ParcelFileDescriptor(java.io.File f, int m) { file = f; mode = m; }
  public static ParcelFileDescriptor open(java.io.File f, int mode) throws java.io.FileNotFoundException {
    if (!f.exists()) throw new java.io.FileNotFoundException(f.toString());
    return new ParcelFileDescriptor(f, mode);
  }
}
