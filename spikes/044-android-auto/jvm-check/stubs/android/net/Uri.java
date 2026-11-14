package android.net;
public final class Uri {
  final String s;
  Uri(String s) { this.s = s; }
  public static Uri parse(String s) { return new Uri(s); }
  public String getLastPathSegment() { return s.substring(s.lastIndexOf('/') + 1); }
  public String toString() { return s; }
}
