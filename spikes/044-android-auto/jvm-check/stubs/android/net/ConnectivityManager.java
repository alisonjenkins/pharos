package android.net;

public class ConnectivityManager {
  public static ConnectivityManager last;
  public NetworkCallback callback;
  public int unregisters;

  public ConnectivityManager() { last = this; }

  public void registerDefaultNetworkCallback(NetworkCallback cb) {
    callback = cb;
    cb.onAvailable(new Network());
  }

  public void unregisterNetworkCallback(NetworkCallback cb) {
    if (cb == callback) { callback = null; unregisters++; }
  }

  public static class NetworkCallback {
    public NetworkCallback() {}
    public void onAvailable(Network network) {}
    public void onLost(Network network) {}
  }
}
