use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;

const MAX_BODY: u64 = 512 * 1024 * 1024;

pub struct Http {
    agent: ureq::Agent,
    user_agent: String,
}

impl Http {
    pub fn new(user_agent: &str) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(120)).build();
        Http {
            agent,
            user_agent: user_agent.to_string(),
        }
    }

    /// Fetches a URL, transparently unpacking gzip files served without Content-Encoding.
    pub fn get(&self, url: &str, headers: &BTreeMap<String, String>) -> Result<Vec<u8>> {
        let mut req = self.agent.get(url).set("User-Agent", &self.user_agent);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        let mut body = Vec::new();
        resp.into_reader().take(MAX_BODY).read_to_end(&mut body)?;
        if body.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::new();
            GzDecoder::new(&body[..])
                .take(MAX_BODY)
                .read_to_end(&mut out)
                .context("gunzip")?;
            body = out;
        }
        Ok(body)
    }

    pub fn get_text(&self, url: &str, headers: &BTreeMap<String, String>) -> Result<String> {
        String::from_utf8(self.get(url, headers)?).with_context(|| format!("{url} is not UTF-8"))
    }
}
