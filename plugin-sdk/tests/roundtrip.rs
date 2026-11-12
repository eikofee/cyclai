use async_trait::async_trait;
use cyclai_plugin_sdk::*;
use std::sync::Arc;

struct Fake;

#[async_trait]
impl Plugin for Fake {
    async fn manifest(&self) -> Manifest {
        Manifest {
            protocol: PROTOCOL_VERSION,
            id: "fake".into(),
            label: "Fake".into(),
            version: "0.0.1".into(),
            sports: vec!["Ride".into()],
            configured: true,
            message: None,
            activity_url: Some("https://example.com/a/{id}".into()),
            activity_url_label: None,
        }
    }
    async fn list(&self, after: i64) -> Result<Vec<ListedActivity>, PluginError> {
        if after == 999 {
            return Err(PluginError::rate_limited("slow down", Some(7)));
        }
        Ok(vec![ListedActivity {
            external_id: format!("a b/{after}"),
            title: "t".into(),
            started_at: "2026-11-10T08:00:00Z".into(),
            start_epoch: 1_000,
            duration_s: None,
            sport_type: "Ride".into(),
        }])
    }
    async fn fetch(&self, id: &str) -> Result<Fetched, PluginError> {
        match id {
            "fit" => Ok(Fetched::Fit(b"FITBYTES".to_vec())),
            "none" => Ok(Fetched::NotAvailable),
            "boom" => Err(PluginError::upstream("source is down")),
            other => Ok(Fetched::Activity(Activity {
                started_at: "2026-11-10T08:00:00Z".into(),
                sport_type: "Ride".into(),
                duration_s: 60,
                distance_m: 1.0,
                elevation_gain_m: 0.0,
                avg_power_w: None,
                normalized_power_w: None,
                avg_hr: None,
                max_hr: None,
                title: other.into(),
                description: None,
                streams: serde_json::json!({"time": {"data": [0]}}),
                laps: vec![],
            })),
        }
    }
}

const SECRET: &str = "a-long-enough-shared-secret";

async fn start(secret: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(Arc::new(Fake), secret);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn cyclai_talks_to_a_plugin_through_signed_requests() {
    let url = start(SECRET).await;
    let client = Client::new(&url, SECRET).unwrap();

    let m = client.manifest().await.unwrap();
    assert_eq!((m.id.as_str(), m.configured), ("fake", true));

    // An id with a space and a slash survives the round trip.
    let listed = client.list(5).await.unwrap();
    assert_eq!(listed[0].external_id, "a b/5");
    match client.fetch(&listed[0].external_id).await.unwrap() {
        Fetched::Activity(a) => assert_eq!(a.title, "a b/5"),
        other => panic!("{other:?}"),
    }
    assert_eq!(client.fetch("fit").await.unwrap(), Fetched::Fit(b"FITBYTES".to_vec()));
    assert_eq!(client.fetch("none").await.unwrap(), Fetched::NotAvailable);

    match client.fetch("boom").await.unwrap_err() {
        ClientError::Rejected { status, message, .. } => assert_eq!((status, message.as_str()), (502, "source is down")),
        e => panic!("{e}"),
    }
    match client.list(999).await.unwrap_err() {
        ClientError::Rejected { status, retry_after_s, .. } => assert_eq!((status, retry_after_s), (429, Some(7))),
        e => panic!("{e}"),
    }
}

#[tokio::test]
async fn a_plugin_refuses_callers_without_the_secret_and_unsigned_requests() {
    let url = start(SECRET).await;
    let wrong = Client::new(&url, "another-long-enough-secret").unwrap();
    match wrong.manifest().await.unwrap_err() {
        ClientError::Rejected { status, .. } => assert_eq!(status, 401),
        e => panic!("{e}"),
    }
    let bare = reqwest::get(format!("{url}/activities?after=0")).await.unwrap();
    assert_eq!(bare.status(), 401);
    let bare = reqwest::get(format!("{url}/manifest")).await.unwrap();
    assert_eq!(bare.status(), 401);
}

#[tokio::test]
async fn short_secrets_and_bad_urls_are_refused_up_front() {
    assert!(matches!(Client::new("http://x", "short"), Err(ClientError::Config(_))));
    assert!(matches!(Client::new("ftp://x", SECRET), Err(ClientError::Config(_))));
    assert!(serve(Arc::new(Fake), "127.0.0.1:0".parse().unwrap(), "short".into()).await.is_err());
}
