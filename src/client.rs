//! Thin HTTP client used by the CLI. Point FLEET51_URL/FLEET51_TOKEN at a remote daemon to drive
//! a fleet on another machine exactly like a local one.

use std::io::{BufRead, BufReader};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::daemon::read_info;
use crate::home::Home;

pub struct Client {
    pub base: String,
    pub token: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(home: &Home) -> Result<Client> {
        let base = match std::env::var("FLEET51_URL") {
            Ok(u) => u,
            Err(_) => read_info(home).map(|i| i.url).context("Fleet-51 daemon is not running (start it with `fleet51 up`)")?,
        };
        let token = match std::env::var("FLEET51_TOKEN") {
            Ok(t) => t,
            Err(_) => home.token()?,
        };
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        Ok(Client { base: base.trim_end_matches('/').to_string(), token, agent })
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn finish(mut resp: ureq::http::Response<ureq::Body>) -> Result<Value> {
        let ok = resp.status().is_success();
        let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        if !ok {
            bail!("{}", v["error"].as_str().unwrap_or("request failed"));
        }
        Ok(v)
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        let resp = self.agent.get(&format!("{}{path}", self.base)).header("Authorization", &self.auth()).call().context("contacting daemon")?;
        Self::finish(resp)
    }

    pub fn post(&self, path: &str, body: Value) -> Result<Value> {
        let resp = self.agent.post(&format!("{}{path}", self.base)).header("Authorization", &self.auth()).send_json(&body).context("contacting daemon")?;
        Self::finish(resp)
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        let resp = self.agent.delete(&format!("{}{path}", self.base)).header("Authorization", &self.auth()).call().context("contacting daemon")?;
        Self::finish(resp)
    }

    /// Read a server-sent event stream; `f` returns false to stop.
    pub fn stream(&self, path: &str, mut f: impl FnMut(Value) -> bool) -> Result<()> {
        let resp = self.agent.get(&format!("{}{path}", self.base)).header("Authorization", &self.auth()).call()?;
        if !resp.status().is_success() {
            return Self::finish(resp).map(|_| ());
        }
        let reader = BufReader::new(resp.into_body().into_with_config().limit(u64::MAX).reader());
        for line in reader.lines() {
            let line = line?;
            if let Some(data) = line.strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                    if !f(v) {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}
