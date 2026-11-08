//! The first contracts: probes and the API 404. Lane J grows this suite.

use ownhealth_contract_tests::Client;

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn healthz_and_readyz() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (s, v) = c.get_json("/healthz").await?;
    assert_eq!((s, v["status"].as_str()), (200, Some("ok")));
    assert!(v["version"].is_string());
    let (s, v) = c.get_json("/readyz").await?;
    assert_eq!((s, v), (200, serde_json::json!({"status": "ready"})));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn unknown_api_paths_are_404_not_found() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (s, v) = c.get_json("/api/no-such-route").await?;
    assert_eq!((s, v["detail"]["code"].as_str()), (404, Some("not_found")));
    Ok(())
}
