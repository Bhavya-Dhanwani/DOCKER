//! `docker search`: query the Docker Hub search API.
use crate::{
    error::{err, Result},
    http,
};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Hit {
    pub repo_name: String,
    #[serde(default)]
    pub short_description: Option<String>,
    #[serde(default)]
    pub star_count: u64,
    #[serde(default)]
    pub is_official: bool,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    results: Vec<Hit>,
}

/// Percent-encode everything but unreserved characters.
fn encode(s: &str) -> String {
    s.bytes().fold(String::new(), |mut o, b| {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
        o
    })
}

fn parse(body: &[u8]) -> Result<Vec<Hit>> {
    Ok(serde_json::from_slice::<Page>(body)?.results)
}

/// Up to `limit` Docker Hub repositories matching `term`, most relevant first.
pub fn search(tmp: &std::path::Path, term: &str, limit: u32) -> Result<Vec<Hit>> {
    if term.trim().is_empty() {
        return Err(err("a search term is required"));
    }
    fs::create_dir_all(tmp)?;
    let dest = http::scratch(tmp, "search");
    let url = format!("https://hub.docker.com/v2/search/repositories/?query={}&page_size={limit}", encode(term.trim()));
    let r = http::send_retry(&http::Req { method: "GET", url: &url, headers: &[], upload: None }, &dest, 3)?;
    let body = fs::read(&dest).unwrap_or_default();
    let _ = fs::remove_file(&dest);
    if r.status != 200 {
        return Err(err(format!("Docker Hub search failed: HTTP {}", r.status)));
    }
    parse(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query() {
        assert_eq!(encode("alpine linux/x"), "alpine%20linux%2Fx");
        assert_eq!(encode("a-b_c.d~"), "a-b_c.d~");
    }

    #[test]
    fn parses_hub_reply() {
        let hits = parse(br#"{"count":2,"results":[
            {"repo_name":"alpine","short_description":"A minimal image","star_count":11000,"is_official":true,"pull_count":1},
            {"repo_name":"u/alp","short_description":null,"star_count":2,"is_official":false}]}"#)
        .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].is_official && hits[0].star_count == 11000);
        assert_eq!(hits[1].short_description, None);
        assert!(parse(b"nope").is_err());
    }
}
