//! The Prometheus endpoint serves what the code records. Alone in its binary: it installs the
//! process-wide recorder.

use std::{net::TcpListener, time::Duration};

#[tokio::test]
async fn the_metrics_endpoint_serves_recorded_metrics() {
    // A free port: bind, read it, release it for the exporter.
    let addr = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    mergetip_runtime::metrics::install(addr).unwrap();

    metrics::counter!("tips_final_total", "state" => "paid").increment(2);
    metrics::histogram!("webhook_ack_seconds", "source" => "github").record(0.03);
    metrics::gauge!("tips_in_state", "state" => "paying").set(1.0);

    let url = format!("http://{addr}/metrics");
    let mut body = String::new();
    for _ in 0..50 {
        if let Ok(response) = reqwest::get(&url).await {
            body = response.text().await.unwrap();
            if body.contains("tips_final_total") {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        body.contains(r#"tips_final_total{state="paid"} 2"#),
        "{body}"
    );
    assert!(
        body.contains(r#"tips_in_state{state="paying"} 1"#),
        "{body}"
    );
    // A real histogram with the configured buckets, so alerts can take quantiles.
    assert!(
        body.contains(r#"webhook_ack_seconds_bucket{source="github",le="0.05"} 1"#),
        "{body}"
    );
}
