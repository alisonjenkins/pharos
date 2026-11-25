//! The server asks Kubernetes for its own rollout restart when its media
//! share breaks, and stays Ready meanwhile (ADR-0156, V1532).
//!
//! Without this, a broken share fails `/readyz` (B1531) and the whole of
//! Pharos is out of its Service until the `selfHeal` reaper notices and a
//! fresh pod is Ready. Browsing, search and the API do not need the share, so
//! that outage is wider than the fault. Instead, the first share fault in a
//! pod that has read the share before sets the Deployment's
//! `kubectl.kubernetes.io/restartedAt`, as `kubectl rollout restart` does, and
//! `/readyz` keeps passing. Under `RollingUpdate` with `maxUnavailable: 0`
//! and `maxSurge` above 0 (the chart's defaults), the old pod serves
//! everything but media until the new one is Ready, provided that happens
//! within [`GIVE_UP_AFTER`]. Other rollout settings let the Deployment stop
//! the old pod first. Under `Recreate` the outage is one restart, not the
//! reaper's wait.
//!
//! Readiness still fails, as before, whenever this cannot help: no
//! restarter configured, the request failed or was skipped (a recent
//! restart, a paused Deployment), the pod never read the share, or
//! [`GIVE_UP_AFTER`] passed without the pod being replaced. The reaper,
//! when `selfHeal` is on, then heals it as it did.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// `pharos_media_share_restart_total{outcome}`: one per restart request.
pub const RESTART_TOTAL: &str = "pharos_media_share_restart_total";

/// How long a pod that asked for its restart stays Ready while not replaced.
/// Past it, `/readyz` fails so the reaper takes over (a rollout that cannot
/// start a Ready pod would otherwise leave this one up for good).
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(15 * 60);

/// A Deployment restarted this recently is not restarted again: its new pod
/// may be the one whose share broke, and another restart would only churn.
pub const RECENT_RESTART: Duration = Duration::from_secs(10 * 60);

/// How far in the future a `restartedAt` may be and still count as recent:
/// node clocks drift a little. One further ahead is ignored, so a badly
/// skewed writer cannot hold off restarts for hours.
pub const CLOCK_SKEW: Duration = Duration::from_secs(60);

/// The annotation `kubectl rollout restart` and the reaper both set.
const RESTARTED_AT: &str = "kubectl.kubernetes.io/restartedAt";

/// What a restart request did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The Deployment's pod template now carries a new `restartedAt`.
    Restarted,
    /// Left alone: it was restarted `age_secs` ago.
    RecentlyRestarted { age_secs: u64 },
    /// Left alone: the Deployment is paused, so a patch would never roll out.
    Paused,
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Restarted => "restarted",
            Self::RecentlyRestarted { .. } => "recently_restarted",
            Self::Paused => "paused",
        }
    }
}

/// The `outcome` label for a request that failed.
pub const FAILED: &str = "failed";

type Request = Pin<Box<dyn Future<Output = Result<Outcome, String>> + Send>>;

/// Something that can restart this pod's Deployment.
pub trait Restarter: Send + Sync {
    /// Restarts the Deployment unless it was restarted within
    /// [`RECENT_RESTART`].
    fn restart(&self) -> Request;
    /// What is restarted, for the log.
    fn target(&self) -> String;
}

#[derive(Debug, Clone)]
enum State {
    Idle,
    Requesting(Instant),
    Requested(Instant),
    NotRestarted,
}

/// The process's restart state.
pub struct SelfRestart {
    restarter: OnceLock<Arc<dyn Restarter>>,
    /// The runtime `set_restarter` ran on, for a fault found off it (the
    /// media share's listing thread).
    runtime: OnceLock<tokio::runtime::Handle>,
    state: Mutex<State>,
    give_up_after: Duration,
}

impl SelfRestart {
    pub fn new(give_up_after: Duration) -> Self {
        Self {
            restarter: OnceLock::new(),
            runtime: OnceLock::new(),
            state: Mutex::new(State::Idle),
            give_up_after,
        }
    }

    pub fn global() -> &'static Self {
        static RESTART: OnceLock<SelfRestart> = OnceLock::new();
        RESTART.get_or_init(|| Self::new(GIVE_UP_AFTER))
    }

    /// Registers how to restart; without one the share fault fails
    /// readiness as before.
    pub fn set_restarter(&self, restarter: Arc<dyn Restarter>) {
        if self.restarter.set(restarter).is_err() {
            tracing::warn!("self-restart: a restarter was already set; keeping the first");
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(self.runtime.set(runtime));
        }
    }

    /// Called once, when the share first breaks. Asks for the restart on
    /// the current runtime, at most once per process.
    pub fn on_share_broken(&'static self, cause: &str) {
        let Some(restarter) = self.restarter.get().cloned() else {
            return;
        };
        let Some(runtime) = tokio::runtime::Handle::try_current()
            .ok()
            .or_else(|| self.runtime.get().cloned())
        else {
            tracing::warn!("self-restart: no runtime to ask for the restart on");
            return;
        };
        {
            let mut state = self.lock();
            if !matches!(*state, State::Idle) {
                return;
            }
            *state = State::Requesting(Instant::now());
        }
        let target = restarter.target();
        let cause = cause.to_owned();
        runtime.spawn(async move {
            let result = restarter.restart().await;
            self.finish(&target, &cause, result);
        });
    }

    fn finish(&self, target: &str, cause: &str, result: Result<Outcome, String>) {
        let label = result.as_ref().map_or(FAILED, Outcome::label);
        metrics::counter!(RESTART_TOTAL, "outcome" => label).increment(1);
        let mut state = self.lock();
        match result {
            Ok(Outcome::Restarted) => {
                let since = match *state {
                    State::Requesting(at) => at,
                    _ => Instant::now(),
                };
                *state = State::Requested(since);
                tracing::warn!(
                    target_deployment = target,
                    outcome = label,
                    cause,
                    "self-restart: asked for a rollout restart; staying ready until replaced"
                );
            }
            Ok(Outcome::RecentlyRestarted { age_secs }) => {
                *state = State::NotRestarted;
                tracing::warn!(
                    target_deployment = target,
                    outcome = label,
                    age_secs,
                    cause,
                    "self-restart: deployment restarted recently; going unready for the reaper"
                );
            }
            Ok(Outcome::Paused) => {
                *state = State::NotRestarted;
                tracing::warn!(
                    target_deployment = target,
                    outcome = label,
                    cause,
                    "self-restart: deployment is paused; going unready for the reaper"
                );
            }
            Err(error) => {
                *state = State::NotRestarted;
                tracing::error!(
                    target_deployment = target,
                    outcome = label,
                    error = %error,
                    cause,
                    "self-restart: restart request failed; going unready for the reaper"
                );
            }
        }
    }

    /// Whether a share fault may leave this pod Ready: a restart was asked
    /// for (or is being asked for) less than `give_up_after` ago.
    pub fn keeps_ready(&self) -> bool {
        match *self.lock() {
            State::Requesting(at) | State::Requested(at) => at.elapsed() < self.give_up_after,
            State::Idle | State::NotRestarted => false,
        }
    }

    /// The `/readyz` note for a share fault this pod stays Ready through.
    pub fn status(&self) -> Option<&'static str> {
        match *self.lock() {
            State::Requesting(_) => Some("requesting"),
            State::Requested(_) => Some("requested"),
            State::Idle | State::NotRestarted => None,
        }
    }

    /// A state that has already asked for its restart (other modules' tests).
    #[cfg(test)]
    pub(crate) fn requested(give_up_after: Duration) -> &'static Self {
        let restart = Box::leak(Box::new(Self::new(give_up_after)));
        *restart.lock() = State::Requested(Instant::now());
        restart
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Restarts a Deployment through the Kubernetes API, as the pod's own
/// service account.
pub struct KubeRestarter {
    http: reqwest::Client,
    api: String,
    token_path: std::path::PathBuf,
    namespace: String,
    deployment: String,
}

/// The service account files every pod gets mounted.
const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// How long one API call may take.
const API_TIMEOUT: Duration = Duration::from_secs(10);

/// How many times a restart reads and patches the Deployment when another
/// writer changed it in between (a 409 on the conditional patch).
const ATTEMPTS: usize = 3;

impl KubeRestarter {
    /// From the environment the chart sets: `PHAROS_SELF_RESTART_DEPLOYMENT`
    /// names the Deployment, `POD_NAMESPACE` its namespace (else the service
    /// account's), and the API is `KUBERNETES_SERVICE_HOST`/`_PORT` with the
    /// service account's CA. `None` when the deployment is not named.
    pub fn from_env() -> Option<Result<Self, String>> {
        let deployment = std::env::var("PHAROS_SELF_RESTART_DEPLOYMENT")
            .ok()
            .filter(|d| !d.is_empty())?;
        Some(Self::in_cluster(deployment))
    }

    fn in_cluster(deployment: String) -> Result<Self, String> {
        let sa = std::path::Path::new(SA_DIR);
        let host = std::env::var("KUBERNETES_SERVICE_HOST")
            .map_err(|_| "KUBERNETES_SERVICE_HOST is not set".to_owned())?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".to_owned());
        let namespace = match std::env::var("POD_NAMESPACE") {
            Ok(ns) if !ns.is_empty() => ns,
            _ => std::fs::read_to_string(sa.join("namespace"))
                .map_err(|e| format!("reading the service account namespace: {e}"))?
                .trim()
                .to_owned(),
        };
        let ca = std::fs::read(sa.join("ca.crt"))
            .map_err(|e| format!("reading the service account CA: {e}"))?;
        let http = client_trusting(&ca)?;
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        Ok(Self {
            http,
            api: format!("https://{host}:{port}"),
            token_path: sa.join("token"),
            namespace,
            deployment,
        })
    }

    /// A restarter against `api` over plain HTTP (tests).
    pub fn with_api(
        api: String,
        token_path: std::path::PathBuf,
        namespace: String,
        deployment: String,
    ) -> Result<Self, String> {
        let http = crate::http_client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            api,
            token_path,
            namespace,
            deployment,
        })
    }

    fn url(&self) -> String {
        format!(
            "{}/apis/apps/v1/namespaces/{}/deployments/{}",
            self.api, self.namespace, self.deployment
        )
    }

    async fn run(
        http: reqwest::Client,
        url: String,
        token_path: std::path::PathBuf,
    ) -> Result<Outcome, String> {
        for attempt in 1..=ATTEMPTS {
            match Self::attempt(&http, &url, &token_path).await? {
                Attempt::Done(outcome) => return Ok(outcome),
                Attempt::Conflict => tracing::info!(
                    attempt,
                    "self-restart: the deployment changed since it was read; reading it again"
                ),
            }
        }
        Err(format!(
            "PATCH deployment: 409 Conflict on all {ATTEMPTS} attempts"
        ))
    }

    /// One read and patch. The patch carries the read's `resourceVersion`,
    /// so a writer in between (the reaper, a rollout) makes it a 409
    /// instead of a second restart decided on stale state.
    async fn attempt(
        http: &reqwest::Client,
        url: &str,
        token_path: &std::path::Path,
    ) -> Result<Attempt, String> {
        // Read each time: projected service account tokens rotate.
        let token = tokio::fs::read_to_string(token_path)
            .await
            .map_err(|e| format!("reading the service account token: {e}"))?;
        let token = token.trim();
        let got = http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("GET deployment: {e}"))?;
        let status = got.status();
        if !status.is_success() {
            return Err(format!("GET deployment: {status}"));
        }
        let deployment: serde_json::Value = got
            .json()
            .await
            .map_err(|e| format!("GET deployment body: {e}"))?;
        let now = unix_now();
        if deployment
            .pointer("/spec/paused")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            return Ok(Attempt::Done(Outcome::Paused));
        }
        if let Some(at) = deployment
            .pointer("/spec/template/metadata/annotations")
            .and_then(|a| a.get(RESTARTED_AT))
            .and_then(serde_json::Value::as_str)
            .and_then(parse_rfc3339)
        {
            if let Some(age_secs) = recent_age(now, at) {
                return Ok(Attempt::Done(Outcome::RecentlyRestarted { age_secs }));
            }
        }
        let version = deployment
            .pointer("/metadata/resourceVersion")
            .and_then(serde_json::Value::as_str)
            .ok_or("GET deployment body: no metadata.resourceVersion")?;
        let patch = serde_json::json!({
            "metadata": { "resourceVersion": version },
            "spec": { "template": { "metadata": { "annotations": {
                RESTARTED_AT: format_rfc3339(now),
            } } } }
        });
        let patched = http
            .patch(url)
            .bearer_auth(token)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/merge-patch+json",
            )
            .body(patch.to_string())
            .send()
            .await
            .map_err(|e| format!("PATCH deployment: {e}"))?;
        let status = patched.status();
        if status == reqwest::StatusCode::CONFLICT {
            return Ok(Attempt::Conflict);
        }
        if !status.is_success() {
            return Err(format!("PATCH deployment: {status}"));
        }
        Ok(Attempt::Done(Outcome::Restarted))
    }
}

/// How long ago `at` was, when it counts as a recent restart: within
/// [`RECENT_RESTART`] before `now`, or at most [`CLOCK_SKEW`] after it. A
/// time further ahead is a skewed clock and is ignored.
fn recent_age(now: i64, at: i64) -> Option<u64> {
    let age = now.saturating_sub(at);
    if age < -(CLOCK_SKEW.as_secs() as i64) {
        tracing::warn!(
            ahead_secs = -age,
            "self-restart: restartedAt is in the future beyond the clock skew allowed; ignoring it"
        );
        return None;
    }
    (age < RECENT_RESTART.as_secs() as i64).then(|| u64::try_from(age.max(0)).unwrap_or(0))
}

/// What one read and patch came to.
enum Attempt {
    Done(Outcome),
    /// The Deployment changed after it was read.
    Conflict,
}

impl Restarter for KubeRestarter {
    fn restart(&self) -> Request {
        Box::pin(Self::run(
            self.http.clone(),
            self.url(),
            self.token_path.clone(),
        ))
    }

    fn target(&self) -> String {
        format!("{}/{}", self.namespace, self.deployment)
    }
}

/// A client that trusts only the cluster CA in `pem`.
fn client_trusting(pem: &[u8]) -> Result<reqwest::Client, String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(pem) {
        let cert = cert.map_err(|e| format!("parsing the service account CA: {e}"))?;
        roots
            .add(cert)
            .map_err(|e| format!("adding the service account CA: {e}"))?;
    }
    if roots.is_empty() {
        return Err("the service account CA holds no certificate".to_owned());
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .tls_backend_preconfigured(config)
        .timeout(API_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days in month `m` (1-12) of year `y`, proleptic Gregorian.
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        4 | 6 | 9 | 11 => 30,
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

/// Unix seconds for an RFC 3339 time such as `2026-11-24T18:00:00Z` or
/// `2026-11-24T19:00:00+01:00` (what `kubectl rollout restart` writes);
/// fractional seconds are ignored.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo)
        || !(1..=days_in_month(y, mo)).contains(&d)
        || h > 23
        || mi > 59
        || sec > 60
    {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let oh = rest.get(1..3)?.parse::<i64>().ok()?;
            let om = rest.get(4..6)?.parse::<i64>().ok()?;
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for unix seconds.
fn format_rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // civil_from_days, the inverse of `days_from_civil`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn rfc3339_round_trips_and_reads_kubectl_offsets() {
        let t = 1_795_543_200; // 2026-11-24T18:00:00Z
        assert_eq!(format_rfc3339(t), "2026-11-24T18:00:00Z");
        assert_eq!(parse_rfc3339("2026-11-24T18:00:00Z"), Some(t));
        assert_eq!(parse_rfc3339("2026-11-24T19:00:00+01:00"), Some(t));
        assert_eq!(parse_rfc3339("2026-11-24T13:00:00-05:00"), Some(t));
        assert_eq!(parse_rfc3339("2026-11-24T18:00:00.123Z"), Some(t));
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert!(parse_rfc3339("2000-02-29T00:00:00Z").is_some());
        assert!(parse_rfc3339("2026-12-31T00:00:00Z").is_some());
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        for bad in [
            "",
            "yesterday",
            "2026-11-24",
            "2026-13-01T00:00:00Z",
            "2026-11-24T18:00:00",
            "2026-11-24T18-00-00Z",
            "2026-11-24T18:00.00Z",
            "2026-11-31T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    struct Fake {
        outcome: Result<Outcome, String>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Restarter for Fake {
        fn restart(&self) -> Request {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let outcome = self.outcome.clone();
            Box::pin(async move { outcome })
        }
        fn target(&self) -> String {
            "pharos/pharos".to_owned()
        }
    }

    fn restart_with(
        outcome: Result<Outcome, String>,
        give_up_after: Duration,
    ) -> (&'static SelfRestart, Arc<std::sync::atomic::AtomicUsize>) {
        let restart: &'static SelfRestart = Box::leak(Box::new(SelfRestart::new(give_up_after)));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        restart.set_restarter(Arc::new(Fake {
            outcome,
            calls: Arc::clone(&calls),
        }));
        (restart, calls)
    }

    async fn settle(restart: &SelfRestart) {
        for _ in 0..100 {
            if !matches!(restart.status(), Some("requesting")) {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    fn restarts(recorder: &metrics_util::debugging::DebuggingRecorder) -> Vec<(String, u64)> {
        use metrics_util::debugging::DebugValue;
        recorder
            .snapshotter()
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == RESTART_TOTAL)
            .map(|(k, _, _, v)| {
                let outcome = k
                    .key()
                    .labels()
                    .find(|l| l.key() == "outcome")
                    .map(|l| l.value().to_owned())
                    .unwrap_or_default();
                match v {
                    DebugValue::Counter(n) => (outcome, n),
                    other => panic!("{other:?}"),
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn a_restart_is_asked_for_once_and_keeps_the_pod_ready() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let (restart, calls) = restart_with(Ok(Outcome::Restarted), GIVE_UP_AFTER);
        assert!(!restart.keeps_ready(), "nothing asked for yet");
        restart.on_share_broken("reading /m/a: Stale file handle");
        restart.on_share_broken("reading /m/b: Stale file handle");
        settle(restart).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(restart.keeps_ready());
        assert_eq!(restart.status(), Some("requested"));
        assert_eq!(restarts(&recorder), vec![("restarted".to_owned(), 1)]);
    }

    #[tokio::test]
    async fn a_failed_or_skipped_restart_leaves_the_pod_to_go_unready() {
        for (outcome, label) in [
            (Err("PATCH deployment: 403 Forbidden".to_owned()), "failed"),
            (
                Ok(Outcome::RecentlyRestarted { age_secs: 30 }),
                "recently_restarted",
            ),
            (Ok(Outcome::Paused), "paused"),
        ] {
            let recorder = metrics_util::debugging::DebuggingRecorder::new();
            let _guard = metrics::set_default_local_recorder(&recorder);
            let (restart, _) = restart_with(outcome, GIVE_UP_AFTER);
            restart.on_share_broken("reading /m/a: Input/output error");
            settle(restart).await;
            assert!(!restart.keeps_ready(), "{label}");
            assert_eq!(restart.status(), None, "{label}");
            assert_eq!(restarts(&recorder), vec![(label.to_owned(), 1)]);
        }
    }

    /// A fault found off the runtime (the share's listing thread) still asks
    /// for the restart, on the runtime the restarter was set on.
    #[tokio::test]
    async fn a_fault_found_off_the_runtime_still_asks_for_the_restart() {
        let (restart, calls) = restart_with(Ok(Outcome::Restarted), GIVE_UP_AFTER);
        std::thread::spawn(move || restart.on_share_broken("listing /m: Input/output error"))
            .join()
            .unwrap();
        settle(restart).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(restart.status(), Some("requested"));
    }

    #[tokio::test]
    async fn a_pod_not_replaced_in_time_stops_staying_ready() {
        let (restart, _) = restart_with(Ok(Outcome::Restarted), Duration::ZERO);
        restart.on_share_broken("reading /m/a: Stale file handle");
        settle(restart).await;
        assert!(!restart.keeps_ready());
    }

    #[tokio::test]
    async fn without_a_restarter_nothing_is_asked_for() {
        let restart: &'static SelfRestart = Box::leak(Box::new(SelfRestart::new(GIVE_UP_AFTER)));
        restart.on_share_broken("reading /m/a: Stale file handle");
        assert!(!restart.keeps_ready());
    }

    /// What the fake API server saw.
    #[derive(Default)]
    struct Seen {
        patches: Vec<(Option<String>, Option<String>, serde_json::Value)>,
        gets: usize,
    }

    struct Api {
        seen: Mutex<Seen>,
        restarted_at: Option<String>,
        /// How many patches answer 409 before one succeeds.
        conflicts: usize,
        paused: bool,
    }

    async fn fake_api(restarted_at: Option<String>) -> (String, Arc<Api>) {
        fake_api_with(restarted_at, 0).await
    }

    async fn fake_api_with(restarted_at: Option<String>, conflicts: usize) -> (String, Arc<Api>) {
        serve(Api {
            seen: Mutex::default(),
            restarted_at,
            conflicts,
            paused: false,
        })
        .await
    }

    async fn serve(api: Api) -> (String, Arc<Api>) {
        use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
        let api = Arc::new(api);
        let state = web::Data::from(Arc::clone(&api));
        let path = "/apis/apps/v1/namespaces/media/deployments/pharos";
        let srv = HttpServer::new(move || {
            App::new()
                .app_data(state.clone())
                .route(
                    path,
                    web::get().to(|d: web::Data<Api>| async move {
                        let gets = {
                            let mut seen = d.seen.lock().unwrap();
                            seen.gets += 1;
                            seen.gets
                        };
                        let annotations = match &d.restarted_at {
                            Some(at) => serde_json::json!({ RESTARTED_AT: at }),
                            None => serde_json::json!({}),
                        };
                        HttpResponse::Ok().json(serde_json::json!({
                            "metadata": { "resourceVersion": gets.to_string() },
                            "spec": {
                                "paused": d.paused,
                                "template": { "metadata": {
                                    "annotations": annotations
                                } }
                            }
                        }))
                    }),
                )
                .route(
                    path,
                    web::patch().to(
                        |req: HttpRequest, body: web::Bytes, d: web::Data<Api>| async move {
                            let header = |name| {
                                req.headers()
                                    .get(name)
                                    .and_then(|v| v.to_str().ok())
                                    .map(str::to_owned)
                            };
                            let mut seen = d.seen.lock().unwrap();
                            seen.patches.push((
                                header(actix_web::http::header::AUTHORIZATION),
                                header(actix_web::http::header::CONTENT_TYPE),
                                serde_json::from_slice(&body).unwrap(),
                            ));
                            if seen.patches.len() <= d.conflicts {
                                HttpResponse::Conflict().json(serde_json::json!({}))
                            } else {
                                HttpResponse::Ok().json(serde_json::json!({}))
                            }
                        },
                    ),
                )
        })
        .bind("127.0.0.1:0")
        .unwrap();
        let port = srv.addrs()[0].port();
        tokio::spawn(srv.run());
        (format!("http://127.0.0.1:{port}"), api)
    }

    fn kube(api: String, dir: &tempfile::TempDir) -> KubeRestarter {
        let token = dir.path().join("token");
        std::fs::write(&token, "sekrit\n").unwrap();
        KubeRestarter::with_api(api, token, "media".into(), "pharos".into()).unwrap()
    }

    #[actix_web::test]
    async fn the_kube_restarter_sets_restarted_at_on_the_pod_template() {
        let (api, seen) = fake_api(Some("2020-01-01T00:00:00Z".into())).await;
        let dir = tempfile::tempdir().unwrap();
        let restarter = kube(api, &dir);
        assert_eq!(restarter.target(), "media/pharos");
        assert_eq!(restarter.restart().await, Ok(Outcome::Restarted));
        let seen = seen.seen.lock().unwrap();
        assert_eq!(seen.gets, 1);
        let [(auth, content_type, patch)] = seen.patches.as_slice() else {
            panic!("one patch expected, got {}", seen.patches.len());
        };
        assert_eq!(auth.as_deref(), Some("Bearer sekrit"));
        assert_eq!(
            content_type.as_deref(),
            Some("application/merge-patch+json")
        );
        let at = patch
            .pointer("/spec/template/metadata/annotations")
            .and_then(|a| a.get(RESTARTED_AT))
            .and_then(serde_json::Value::as_str)
            .expect("restartedAt in the patch");
        let age = unix_now() - parse_rfc3339(at).expect("an RFC 3339 time");
        assert!((0..60).contains(&age), "{at}");
        assert_eq!(
            patch.pointer("/metadata/resourceVersion"),
            Some(&serde_json::json!("1")),
            "the patch is conditional on the version read"
        );
    }

    /// A writer between the read and the patch gives a 409: read again and
    /// patch on the new version, a bounded number of times.
    #[actix_web::test]
    async fn the_kube_restarter_rereads_after_a_conflict_and_gives_up_in_bounds() {
        let (api, seen) = fake_api_with(None, 1).await;
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(kube(api, &dir).restart().await, Ok(Outcome::Restarted));
        {
            let seen = seen.seen.lock().unwrap();
            assert_eq!(seen.gets, 2);
            let versions: Vec<_> = seen
                .patches
                .iter()
                .map(|(.., p)| p.pointer("/metadata/resourceVersion").cloned())
                .collect();
            assert_eq!(
                versions,
                [Some(serde_json::json!("1")), Some(serde_json::json!("2"))]
            );
        }

        let (api, seen) = fake_api_with(None, usize::MAX).await;
        let outcome = kube(api, &dir).restart().await;
        assert!(
            matches!(&outcome, Err(e) if e.contains("409")),
            "{outcome:?}"
        );
        assert_eq!(seen.seen.lock().unwrap().patches.len(), ATTEMPTS);
    }

    #[actix_web::test]
    async fn the_kube_restarter_leaves_a_recently_restarted_deployment_alone() {
        let recent = format_rfc3339(unix_now() - 30);
        let (api, seen) = fake_api(Some(recent)).await;
        let dir = tempfile::tempdir().unwrap();
        let outcome = kube(api, &dir).restart().await;
        assert!(
            matches!(outcome, Ok(Outcome::RecentlyRestarted { age_secs }) if age_secs < 60),
            "{outcome:?}"
        );
        assert!(seen.seen.lock().unwrap().patches.is_empty());
    }

    /// A paused Deployment would take the patch and never roll out: skip it
    /// so the pod goes unready for the reaper instead of waiting 15 minutes.
    #[actix_web::test]
    async fn the_kube_restarter_skips_a_paused_deployment() {
        let (api, seen) = serve(Api {
            seen: Mutex::default(),
            restarted_at: None,
            conflicts: 0,
            paused: true,
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(kube(api, &dir).restart().await, Ok(Outcome::Paused));
        assert!(seen.seen.lock().unwrap().patches.is_empty());
    }

    /// A restartedAt a little ahead (clock drift) is recent; one far ahead
    /// is ignored rather than holding off restarts until then.
    #[test]
    fn a_future_restarted_at_counts_only_within_the_clock_skew() {
        let now = 1_795_543_200;
        assert_eq!(recent_age(now, now - 30), Some(30));
        assert_eq!(recent_age(now, now + 30), Some(0));
        assert_eq!(recent_age(now, now + 3 * 3600), None);
        assert_eq!(recent_age(now, now - 11 * 60), None);
    }

    #[actix_web::test]
    async fn the_kube_restarter_reports_a_refused_call() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing listens on the discard port's neighbour; the GET fails.
        let restarter = kube("http://127.0.0.1:9".into(), &dir);
        let outcome = restarter.restart().await;
        assert!(
            matches!(&outcome, Err(e) if e.starts_with("GET deployment")),
            "{outcome:?}"
        );
    }

    #[test]
    fn outcome_labels_are_distinct() {
        let labels = [
            Outcome::Restarted.label(),
            Outcome::RecentlyRestarted { age_secs: 1 }.label(),
            Outcome::Paused.label(),
            FAILED,
        ];
        assert_eq!(
            labels,
            ["restarted", "recently_restarted", "paused", "failed"]
        );
    }
}
