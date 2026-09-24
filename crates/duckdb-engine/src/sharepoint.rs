//! SharePoint Server (on premises: 2016, 2019, Subscription Edition) over its
//! REST API, signed in with Windows authentication (NTLM).
//!
//! Lists read as rows, following `odata.nextLink` page by page, and rows write
//! as list items. Files in a document library read as rows by their format,
//! and a sink's output uploads as a file. NTLM itself is the `ntlmclient`
//! crate's; what is here is the HTTP around it.
//!
//! NTLM signs in a TCP connection rather than a request, so a site holds an
//! agent with a connection pool of its own (`tls::http_agent_unshared`): the
//! connection that finished the handshake is the one the next request uses,
//! and every response is read to its end so the connection goes back to the
//! pool rather than being dropped.

use crate::*;
use base64::Engine as _;

/// A request of one run against one site: the agent that holds the signed-in
/// connection, and the form digest writes need.
struct Site {
    agent: ureq::Agent,
    /// The site URL, without a trailing slash.
    base: String,
    user: String,
    creds: ntlmclient::Credentials,
    digest: Option<(String, std::time::Instant, u64)>,
}

/// `DOMAIN\user`, or `user@domain.example`. The second form is sent whole as
/// the user name with no domain, which is how Windows itself sends a UPN.
fn ntlm_credentials(username: &str, password: &str) -> Result<ntlmclient::Credentials, EngineError> {
    let username = username.trim();
    if username.is_empty() {
        return Err(EngineError::Config("sharepoint: username required (DOMAIN\\user or user@domain)".into()));
    }
    let (domain, user) = match username.split_once('\\') {
        Some((d, u)) => (d.to_string(), u.to_string()),
        None => (String::new(), username.to_string()),
    };
    Ok(ntlmclient::Credentials { username: user, password: password.to_string(), domain })
}

/// Percent-encode everything but the unreserved characters and `keep`.
fn encode(s: &str, keep: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "-._~".contains(c) || (c.is_ascii() && keep.contains(c)) {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// A string inside an OData literal: `'` doubles, then it is made safe for a
/// URL path. Quotes, parentheses and slashes stay as SharePoint writes them.
fn odata_path(s: &str) -> String {
    encode(&s.replace('\'', "''"), "'()/!*,;=:@")
}

/// SharePoint's own words for a failed request: `odata.error.message.value`
/// in the JSON it answers with, else the start of the body.
fn sharepoint_message(body: &str) -> String {
    serde_json::from_str::<JsonValue>(body)
        .ok()
        .and_then(|v| v.pointer("/odata.error/message/value").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(300).collect())
}

impl Site {
    fn new(site_url: &str, username: &str, password: &str) -> Result<Self, EngineError> {
        let base = site_url.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(EngineError::Config(format!(
                "sharepoint: siteUrl must be an http(s) URL, like https://sharepoint.example/sites/team, not {:?}",
                site_url
            )));
        }
        Ok(Site {
            agent: tls::http_agent_unshared(&tls::HttpTransport::default()),
            base,
            user: username.trim().to_string(),
            creds: ntlm_credentials(username, password)?,
            digest: None,
        })
    }

    fn api(&self, path: &str) -> String {
        format!("{}/_api/{}", self.base, path)
    }

    /// One request, signing the connection in first when the server asks.
    ///
    /// Sent as it is first: on a connection that is already signed in, that is
    /// the whole exchange. A 401 that offers NTLM means the connection is not,
    /// so the handshake runs on it - the negotiate leg with no body, which the
    /// server refuses before acting on anything, then the request itself again,
    /// carrying the answer to the server's challenge. Either way the body is
    /// acted on once.
    fn send(&mut self, method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<ureq::Response, EngineError> {
        let request = |auth: Option<&str>, body: &[u8]| {
            let mut r = self.agent.request(method, url);
            for (k, v) in headers {
                r = r.set(k, v);
            }
            if let Some(a) = auth {
                r = r.set("Authorization", a);
            }
            r.send_bytes(body)
        };
        let offered = match request(None, body) {
            Err(ureq::Error::Status(401, r)) => {
                let schemes: Vec<String> = r.all("www-authenticate").iter().map(|s| s.to_string()).collect();
                let _ = r.into_string();
                schemes
            }
            other => return self.checked(other, method, url),
        };
        let scheme = if offered.iter().any(|s| s.eq_ignore_ascii_case("NTLM") || s.starts_with("NTLM ")) {
            "NTLM"
        } else if offered.iter().any(|s| s.eq_ignore_ascii_case("Negotiate") || s.starts_with("Negotiate ")) {
            "Negotiate"
        } else {
            return Err(EngineError::Query(format!(
                "sharepoint: {} asked for a sign-in this node does not do (it offered {:?}); \
                 only Windows authentication (NTLM) is supported",
                url, offered
            )));
        };

        let negotiate = ntlmclient::Message::Negotiate(ntlmclient::NegotiateMessage {
            flags: ntlmclient::Flags::NEGOTIATE_UNICODE
                | ntlmclient::Flags::REQUEST_TARGET
                | ntlmclient::Flags::NEGOTIATE_NTLM,
            supplied_domain: String::new(),
            supplied_workstation: String::new(),
            os_version: Default::default(),
        })
        .to_bytes()
        .map_err(|e| EngineError::Query(format!("sharepoint: NTLM negotiate message: {:?}", e)))?;
        let b64 = base64::engine::general_purpose::STANDARD;
        let challenge = match request(Some(&format!("{} {}", scheme, b64.encode(&negotiate))), &[]) {
            Err(ureq::Error::Status(401, r)) => {
                let token = r
                    .all("www-authenticate")
                    .iter()
                    .find_map(|h| h.strip_prefix(scheme).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string));
                let _ = r.into_string();
                token.ok_or_else(|| {
                    EngineError::Query(format!(
                        "sharepoint: {} did not answer the NTLM negotiate with a challenge; a proxy or \
                         load balancer that does not keep the connection open breaks Windows sign-in",
                        url
                    ))
                })?
            }
            other => return self.checked(other, method, url),
        };
        let challenge = b64
            .decode(challenge.as_bytes())
            .map_err(|e| EngineError::Query(format!("sharepoint: NTLM challenge is not base64: {}", e)))?;
        let challenge = match ntlmclient::Message::try_from(challenge.as_slice()) {
            Ok(ntlmclient::Message::Challenge(c)) => c,
            Ok(other) => {
                return Err(EngineError::Query(format!(
                    "sharepoint: expected an NTLM challenge, got message type {}",
                    other.message_number()
                )))
            }
            Err(e) => return Err(EngineError::Query(format!("sharepoint: NTLM challenge: {:?}", e))),
        };
        let target_info: Vec<u8> = challenge.target_information.iter().flat_map(|ie| ie.to_bytes()).collect();
        let answer = ntlmclient::respond_challenge_ntlm_v2(
            challenge.challenge,
            &target_info,
            ntlmclient::get_ntlm_time(),
            &self.creds,
        )
        .to_message(
            &self.creds,
            "",
            ntlmclient::Flags::NEGOTIATE_UNICODE | ntlmclient::Flags::NEGOTIATE_NTLM,
        )
        .to_bytes()
        .map_err(|e| EngineError::Query(format!("sharepoint: NTLM authenticate message: {:?}", e)))?;
        let result = request(Some(&format!("{} {}", scheme, b64.encode(&answer))), body);
        self.checked(result, method, url)
    }

    /// A response that is not a success, as an error in SharePoint's words.
    fn checked(
        &self,
        result: Result<ureq::Response, ureq::Error>,
        method: &str,
        url: &str,
    ) -> Result<ureq::Response, EngineError> {
        match result {
            Ok(r) => Ok(r),
            Err(ureq::Error::Status(401, r)) => {
                let _ = r.into_string();
                Err(EngineError::Query(format!(
                    "sharepoint: the server refused the sign-in for {} (HTTP 401). Check the user name \
                     (DOMAIN\\user or user@domain) and the password",
                    self.user
                )))
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                Err(EngineError::Query(format!(
                    "sharepoint: {} {} answered HTTP {}: {}",
                    method,
                    url,
                    code,
                    sharepoint_message(&body)
                )))
            }
            Err(e) => Err(EngineError::Query(format!("sharepoint: {} {}: {}", method, url, e))),
        }
    }

    fn json(&mut self, method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<JsonValue, EngineError> {
        let mut all = vec![("Accept", "application/json;odata=nometadata")];
        all.extend_from_slice(headers);
        let text = self
            .send(method, url, &all, body)?
            .into_string()
            .map_err(|e| EngineError::Query(format!("sharepoint: read {}: {}", url, e)))?;
        if text.trim().is_empty() {
            return Ok(JsonValue::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| EngineError::Query(format!("sharepoint: {} answered something that is not JSON: {}", url, e)))
    }

    /// The form digest every write carries. Kept for its lifetime less a
    /// minute, then asked for again.
    fn digest(&mut self) -> Result<String, EngineError> {
        if let Some((value, at, secs)) = &self.digest {
            if at.elapsed().as_secs() + 60 < *secs {
                return Ok(value.clone());
            }
        }
        let url = self.api("contextinfo");
        let info = self.json("POST", &url, &[], &[])?;
        let value = info
            .get("FormDigestValue")
            .and_then(|v| v.as_str())
            .ok_or_else(|| EngineError::Query(format!("sharepoint: {} gave no FormDigestValue", url)))?
            .to_string();
        let secs = info.get("FormDigestTimeoutSeconds").and_then(|v| v.as_u64()).unwrap_or(1800);
        self.digest = Some((value.clone(), std::time::Instant::now(), secs));
        Ok(value)
    }

    /// Every item of a list, page by page through `odata.nextLink`.
    fn list_items(
        &mut self,
        list: &str,
        select: Option<&str>,
        filter: Option<&str>,
        page_size: u64,
    ) -> Result<(Vec<JsonValue>, usize), EngineError> {
        let mut url = format!(
            "{}?$top={}",
            self.api(&format!("web/lists/GetByTitle('{}')/items", odata_path(list))),
            page_size
        );
        if let Some(s) = select {
            url.push_str(&format!("&$select={}", encode(s, ",/")));
        }
        if let Some(f) = filter {
            url.push_str(&format!("&$filter={}", encode(f, "'")));
        }
        let mut rows = Vec::new();
        let mut pages = 0usize;
        let mut seen = std::collections::HashSet::new();
        let mut next = Some(url);
        while let Some(url) = next.take() {
            if !seen.insert(url.clone()) {
                return Err(EngineError::Query(format!(
                    "sharepoint: list {:?} sent the same next page twice ({}); stopped rather than loop",
                    list, url
                )));
            }
            let page = self.json("GET", &url, &[], &[])?;
            pages += 1;
            if let Some(items) = page.get("value").and_then(|v| v.as_array()) {
                rows.extend(items.iter().cloned());
            }
            next = page
                .get("odata.nextLink")
                .or_else(|| page.get("@odata.nextLink"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
        Ok((rows, pages))
    }

    fn add_item(&mut self, list: &str, item: &JsonValue) -> Result<(), EngineError> {
        let url = self.api(&format!("web/lists/GetByTitle('{}')/items", odata_path(list)));
        let body = serde_json::to_vec(item).unwrap_or_default();
        let digest = self.digest()?;
        self.json(
            "POST",
            &url,
            &[("Content-Type", "application/json;odata=nometadata"), ("X-RequestDigest", &digest)],
            &body,
        )?;
        Ok(())
    }

    fn download(&mut self, server_relative_url: &str) -> Result<Vec<u8>, EngineError> {
        let url = self.api(&format!("web/GetFileByServerRelativeUrl('{}')/$value", odata_path(server_relative_url)));
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut self.send("GET", &url, &[], &[])?.into_reader(), &mut bytes)
            .map_err(|e| EngineError::Query(format!("sharepoint: read {}: {}", server_relative_url, e)))?;
        Ok(bytes)
    }

    fn upload(&mut self, folder: &str, name: &str, bytes: &[u8], overwrite: bool) -> Result<(), EngineError> {
        let url = self.api(&format!(
            "web/GetFolderByServerRelativeUrl('{}')/Files/add(url='{}',overwrite={})",
            odata_path(folder),
            odata_path(name),
            overwrite
        ));
        let digest = self.digest()?;
        self.json("POST", &url, &[("X-RequestDigest", &digest)], bytes)?;
        Ok(())
    }
}

/// The DuckDB reader for a file, by the format given or its extension.
fn file_format(name: &str, format: Option<&str>) -> Result<&'static str, EngineError> {
    let ext = format
        .map(|f| f.trim().trim_start_matches('.').to_lowercase())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| name.rsplit('.').next().unwrap_or("").to_lowercase());
    Ok(match ext.as_str() {
        "csv" | "txt" => "csv",
        "tsv" => "tsv",
        "parquet" => "parquet",
        "json" | "jsonl" | "ndjson" => "json",
        "xlsx" => "xlsx",
        other => {
            return Err(EngineError::Config(format!(
                "sharepoint: cannot tell how to read or write {:?} (format {:?}); set format to csv, tsv, \
                 parquet, json or xlsx",
                name, other
            )))
        }
    })
}

fn temp_file(node_id: &str, format: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "duckle-sharepoint-{}-{}-{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        node_id.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
        format
    ))
}

impl DuckdbEngine {
    pub(crate) fn run_sharepoint_source(
        &self,
        db: &Path,
        spec: &plan::SharePointSourceSpec,
    ) -> Result<String, EngineError> {
        let mut site = Site::new(&spec.site_url, &spec.username, &spec.password)?;
        match &spec.read {
            plan::SharePointRead::List { list, select, filter } => {
                let (rows, pages) =
                    site.list_items(list, select.as_deref(), filter.as_deref(), spec.page_size.max(1))?;
                materialize_jsonobjects_as_table(&self.bin, db, &spec.node_id, &rows)?;
                Ok(format!(
                    "sharepoint: read {} items of list {:?} in {} page(s) into {}",
                    rows.len(),
                    list,
                    pages,
                    spec.node_id
                ))
            }
            plan::SharePointRead::File { url, format } => {
                let format = file_format(url, format.as_deref())?;
                let bytes = site.download(url)?;
                let tmp = temp_file(&spec.node_id, format);
                std::fs::write(&tmp, &bytes)
                    .map_err(|e| EngineError::Query(format!("sharepoint: write {}: {}", tmp.display(), e)))?;
                let path = tmp.to_string_lossy().replace('\\', "/").replace('\'', "''");
                let read = match format {
                    "csv" => format!("read_csv_auto('{}')", path),
                    "tsv" => format!("read_csv_auto('{}', delim = '\t')", path),
                    "parquet" => format!("read_parquet('{}')", path),
                    "json" => format!("read_json_auto('{}')", path),
                    _ => format!("read_xlsx('{}')", path),
                };
                let load = if format == "xlsx" { "LOAD excel; " } else { "" };
                let result = self.run(
                    Some(db),
                    &format!(
                        "{}CREATE OR REPLACE TABLE {} AS SELECT * FROM {};",
                        load,
                        plan::quote_ident(&spec.node_id),
                        read
                    ),
                    false,
                );
                let _ = std::fs::remove_file(&tmp);
                result?;
                Ok(format!("sharepoint: read {} ({} bytes) into {}", url, bytes.len(), spec.node_id))
            }
        }
    }

    pub(crate) fn run_sharepoint_sink(
        &self,
        db: &Path,
        spec: &plan::SharePointSinkSpec,
    ) -> Result<String, EngineError> {
        let view = plan::quote_ident(&spec.from_view);
        match &spec.write {
            plan::SharePointWrite::List { list } => {
                // Every field as the JSON type SharePoint takes for it. A
                // decimal would otherwise arrive as a string, which SharePoint
                // parses in the site's locale ("4.5" is 45 on a German site),
                // and a Number column is a double there anyway. Date/times go
                // as ISO 8601, the form the REST API reads.
                let select = describe_columns(self, db, &spec.from_view)
                    .iter()
                    .map(|(name, ty)| {
                        let q = plan::quote_ident(name);
                        let t = ty.to_uppercase();
                        if t.starts_with("DECIMAL") || t == "HUGEINT" || t == "UHUGEINT" {
                            format!("CAST({q} AS DOUBLE) AS {q}")
                        } else if t.contains("WITH TIME ZONE") {
                            format!("strftime({q} AT TIME ZONE 'UTC', '%Y-%m-%dT%H:%M:%SZ') AS {q}")
                        } else if t.starts_with("TIMESTAMP") {
                            format!("strftime({q}, '%Y-%m-%dT%H:%M:%S') AS {q}")
                        } else if t == "DATE" {
                            format!("strftime({q}, '%Y-%m-%d') AS {q}")
                        } else {
                            q
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let rows = self.run_rows(Some(db), &format!("SELECT {} FROM {}", select, view))?;
                if rows.is_empty() {
                    return Ok(format!("sharepoint: 0 rows to add to list {:?}", list));
                }
                let mut site = Site::new(&spec.site_url, &spec.username, &spec.password)?;
                for (i, row) in rows.iter().enumerate() {
                    self.check_cancelled()?;
                    site.add_item(list, row).map_err(|e| {
                        EngineError::Query(format!("{} (row {} of {}; the rows before it were added)", e, i + 1, rows.len()))
                    })?;
                }
                Ok(format!("sharepoint: added {} items to list {:?}", rows.len(), list))
            }
            plan::SharePointWrite::File { folder, name, format, overwrite } => {
                let format = file_format(name, format.as_deref())?;
                // Nothing upstream leaves the library file as it was, rather
                // than replacing it with an empty one.
                let count = self
                    .run_rows(Some(db), &format!("SELECT count(*) AS n FROM {}", view))?
                    .first()
                    .and_then(|r| r.get("n").and_then(|n| n.as_u64()))
                    .unwrap_or(0);
                if count == 0 {
                    return Ok(format!("sharepoint: 0 rows; left {}/{} as it was", folder, name));
                }
                let tmp = temp_file(&spec.from_view, format);
                let path = tmp.to_string_lossy().replace('\\', "/").replace('\'', "''");
                let copy = match format {
                    "csv" => "(FORMAT csv, HEADER true)",
                    "tsv" => "(FORMAT csv, HEADER true, DELIMITER '\t')",
                    "parquet" => "(FORMAT parquet)",
                    "json" => "(FORMAT json)",
                    _ => "(FORMAT xlsx, HEADER true)",
                };
                let load = if format == "xlsx" { "LOAD excel; " } else { "" };
                let written = self.run(Some(db), &format!("{}COPY (SELECT * FROM {}) TO '{}' {};", load, view, path, copy), false);
                let bytes = written.and_then(|_| {
                    std::fs::read(&tmp).map_err(|e| EngineError::Query(format!("sharepoint: read {}: {}", tmp.display(), e)))
                });
                let _ = std::fs::remove_file(&tmp);
                let bytes = bytes?;
                let mut site = Site::new(&spec.site_url, &spec.username, &spec.password)?;
                site.upload(folder, name, &bytes, *overwrite)?;
                Ok(format!("sharepoint: uploaded {} rows as {}/{} ({} bytes)", count, folder, name, bytes.len()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_name_carries_its_domain_either_way() {
        let c = ntlm_credentials(r"CONTOSO\alice", "pw").unwrap();
        assert_eq!((c.domain.as_str(), c.username.as_str()), ("CONTOSO", "alice"));
        let c = ntlm_credentials("alice@contoso.example", "pw").unwrap();
        assert_eq!((c.domain.as_str(), c.username.as_str()), ("", "alice@contoso.example"));
        assert!(ntlm_credentials("  ", "pw").is_err());
    }

    #[test]
    fn a_title_with_a_quote_and_a_space_stays_one_odata_literal() {
        assert_eq!(odata_path("Zoë's list"), "Zo%C3%AB''s%20list");
        assert_eq!(odata_path("/sites/team/Shared Documents/a#1.csv"), "/sites/team/Shared%20Documents/a%231.csv");
    }

    #[test]
    fn sharepoint_says_what_went_wrong_in_its_own_words() {
        let body = r#"{"odata.error":{"code":"-1, System.ArgumentException","message":{"lang":"en-US","value":"List 'X' does not exist."}}}"#;
        assert_eq!(sharepoint_message(body), "List 'X' does not exist.");
        assert_eq!(sharepoint_message("plain text"), "plain text");
    }

    #[test]
    fn a_file_is_read_by_its_format_or_its_extension() {
        assert_eq!(file_format("/a/b.CSV", None).unwrap(), "csv");
        assert_eq!(file_format("/a/b.data", Some("parquet")).unwrap(), "parquet");
        assert_eq!(file_format("/a/b.jsonl", None).unwrap(), "json");
        assert!(file_format("/a/b.docx", None).is_err());
    }
}
