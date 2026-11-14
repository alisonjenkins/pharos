package android.content.res;
public class Resources {
  /** Resolves the app's status icon, as an APK built with the Pharos resources does. */
  public int getIdentifier(String name, String type, String pkg) { return "ic_stat_pharos".equals(name) && "drawable".equals(type) ? 0x7f080001 : 0; }
}
