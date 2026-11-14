package android.content.pm;
public class PackageManager {
  /** Packages per uid, and the system-image packages, as the check sets them. */
  public static final java.util.Map<Integer, String[]> packages = new java.util.HashMap<>();
  public static final java.util.Set<String> system = new java.util.HashSet<>();
  public static class NameNotFoundException extends Exception {}
  public String[] getPackagesForUid(int uid) { return packages.get(uid); }
  public ApplicationInfo getApplicationInfo(String name, int flags) throws NameNotFoundException {
    boolean known = false;
    for (String[] p : packages.values()) for (String n : p) known |= n.equals(name);
    if (!known) throw new NameNotFoundException();
    ApplicationInfo i = new ApplicationInfo();
    i.flags = system.contains(name) ? ApplicationInfo.FLAG_SYSTEM : 0;
    return i;
  }
}
