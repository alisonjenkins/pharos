package android.app;
public abstract class Service extends android.content.Context {
  public void onCreate() { System.out.println("stub: Service.onCreate"); }
  public void onDestroy() { System.out.println("stub: Service.onDestroy"); }
}
