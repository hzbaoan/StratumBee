use std::time::Duration;

use anyhow::{anyhow, Context};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
pub struct RpcClient {
    url: String,
    user: String,
    pass: String,
    client: Client,
    client_longpoll: Client,
}

#[derive(Debug, Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcClient {
    pub fn new(url: String, user: String, pass: String) -> anyhow::Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(8))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(8)
            .build()
            .context("build reqwest client")?;

        let client_longpoll = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(130))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(2)
            .build()
            .context("build longpoll reqwest client")?;

        Ok(Self {
            url,
            user,
            pass,
            client,
            client_longpoll,
        })
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<T> {
        self.call_with_client(&self.client, method, params).await
    }

    pub async fn call_optional<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<Option<T>> {
        let payload = json!({
            "jsonrpc": "1.0",
            "id": "StratumBee",
            "method": method,
            "params": params,
        });

        let response = self
            .client
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.pass))
            .json(&payload)
            .send()
            .await
            .context("RPC request failed")?;

        let body = response.json::<RpcResponse<T>>().await?;
        if let Some(err) = body.error {
            return Err(anyhow!("RPC error {method}: {} ({})", err.message, err.code));
        }

        Ok(body.result)
    }

    pub async fn call_longpoll<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<T> {
        self.call_with_client(&self.client_longpoll, method, params).await
    }

    async fn call_with_client<T: DeserializeOwned>(
        &self,
        client: &Client,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<T> {
        let payload = json!({
            "jsonrpc": "1.0",
            "id": "StratumBee",
            "method": method,
            "params": params,
        });

        let response = client
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.pass))
            .json(&payload)
            .send()
            .await
            .context("RPC request failed")?;

        let status = response.status();
        let body = response.json::<RpcResponse<T>>().await?;

        if let Some(err) = body.error {
            return Err(anyhow!("RPC error {method}: {} ({})", err.message, err.code));
        }

        body.result
            .ok_or_else(|| anyhow!("RPC {method} returned empty result (status {status})"))
    }
}
