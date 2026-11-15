use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;

const MAX_BODY: u64 = 512 * 1024 * 1024;

struct Cached {
    etag: Option<String>,
    last_modified: Option<String>,
    body: Vec<u8>,
}

pub struct Http {
    agent: ureq::Agent,
    user_agent: String,
    cache: Mutex<HashMap<String, Cached>>,
}

impl Http {
    pub fn new(user_agent: &str) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(120)).build();
        Http {
            agent,
            user_agent: user_agent.to_string(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Fetches a URL with a conditional request when a previous response is known, unpacking gzip files
    /// served without Content-Encoding.
    pub fn get(&self, url: &str, headers: &BTreeMap<String, String>) -> Result<Vec<u8>> {
        let mut req = self.agent.get(url).set("User-Agent", &self.user_agent);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        if let Some(c) = self.cache.lock().unwrap().get(url) {
            // Apache appends -gzip to ETags of compressed responses and then never matches them, so
            // If-None-Match would turn every request into a 200 for servers like bison-fute.
            match (&c.last_modified, &c.etag) {
                (Some(lm), _) => req = req.set("If-Modified-Since", lm),
                (None, Some(etag)) => req = req.set("If-None-Match", etag),
                (None, None) => {}
            }
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        if resp.status() == 304 {
            log::debug!("{url} not modified");
            return match self.cache.lock().unwrap().get(url) {
                Some(c) => Ok(c.body.clone()),
                None => bail!("GET {url}: unexpected 304"),
            };
        }
        let etag = resp.header("ETag").map(str::to_string);
        let last_modified = resp.header("Last-Modified").map(str::to_string);
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
        let mut cache = self.cache.lock().unwrap();
        if etag.is_some() || last_modified.is_some() {
            cache.insert(
                url.to_string(),
                Cached {
                    etag,
                    last_modified,
                    body: body.clone(),
                },
            );
        } else {
            cache.remove(url);
        }
        Ok(body)
    }

    /// Drops remembered responses for every URL starting with `prefix`.
    pub fn forget(&self, prefix: &str) {
        self.cache.lock().unwrap().retain(|url, _| !url.starts_with(prefix));
    }

    pub fn get_text(&self, url: &str, headers: &BTreeMap<String, String>) -> Result<String> {
        String::from_utf8(self.get(url, headers)?).with_context(|| format!("{url} is not UTF-8"))
    }
}
