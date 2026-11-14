package android.content;
public class Intent {
  public final Class<?> target;
  public Intent(Context c, Class<?> cls) { target = cls; }
}
