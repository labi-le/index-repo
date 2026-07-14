use crate::store::{CollectionInfo, Meta, Record, Store};
use anyhow::{bail, Result};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

/// Pagination page size for `/get` calls.
const PAGE: usize = 10_000;

// ---------------------------------------------------------------------------
// URL helpers (unit-testable, no network)
// ---------------------------------------------------------------------------

pub fn base_url(host: &str, port: u16, ssl: bool) -> String {
    let scheme = if ssl { "https" } else { "http" };
    format!("{scheme}://{host}:{port}/api/v2")
}

pub fn collections_path(base: &str) -> String {
    format!("{base}/tenants/default_tenant/databases/default_database/collections")
}

/// Build the `Authorization` header value for a ChromaDB static token.
///
/// Returns `None` for an empty/whitespace token or one carrying illegal header
/// bytes, so a malformed token disables auth rather than panicking the client.
fn auth_header_value(token: &str) -> Option<HeaderValue> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    HeaderValue::from_str(&format!("Bearer {token}")).ok()
}

/// Parse the ChromaDB list-collections response into `CollectionInfo`s,
/// extracting the `index_repo` marker and `last_indexed` from each collection's
/// metadata (absent/null metadata → marker false, timestamp None).
fn parse_collection_list(v: &Value) -> Vec<CollectionInfo> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|c| {
            let id = c.get("id")?.as_str()?.to_string();
            let name = c.get("name")?.as_str()?.to_string();
            let meta = c.get("metadata");
            let index_repo = meta
                .and_then(|m| m.get("index_repo"))
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let last_indexed = meta
                .and_then(|m| m.get("last_indexed"))
                .and_then(|n| n.as_u64());
            Some(CollectionInfo {
                id,
                name,
                index_repo,
                last_indexed,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// HttpStore
// ---------------------------------------------------------------------------

pub struct HttpStore {
    client: Client,
    base: String,
    collection_id: Option<String>,
}

impl HttpStore {
    pub fn new(host: &str, port: u16, ssl: bool) -> Self {
        let mut builder = Client::builder();
        // Optional static-token auth (`Authorization: Bearer <token>`) from
        // INDEX_REPO_CHROMA_TOKEN. Absent → unauthenticated (the LAN default).
        if let Some(value) = std::env::var("INDEX_REPO_CHROMA_TOKEN")
            .ok()
            .and_then(|t| auth_header_value(&t))
        {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
        let client = builder.build().expect("failed to build reqwest client");
        Self {
            client,
            base: base_url(host, port, ssl),
            collection_id: None,
        }
    }

    fn col_id(&self) -> Result<&str> {
        self.collection_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no collection selected — call get_or_create first"))
    }

    fn col_url(&self) -> Result<String> {
        Ok(format!(
            "{}/{}",
            collections_path(&self.base),
            self.col_id()?
        ))
    }

    /// Check response status; on failure bail with status + body text.
    fn check(resp: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("HTTP {status}: {body}");
        }
    }
}

// ---------------------------------------------------------------------------
// Store trait impl
// ---------------------------------------------------------------------------

impl Store for HttpStore {
    /// GET /heartbeat — verify reachability; used by main for exit code 3.
    fn heartbeat(&self) -> Result<()> {
        let url = format!("{}/heartbeat", self.base);
        let resp = self.client.get(&url).send()?;
        Self::check(resp)?;
        Ok(())
    }

    /// POST /collections  body: {name, get_or_create:true, configuration:{hnsw:{space:cosine}}}
    /// Stores the returned uuid in self.collection_id.
    fn get_or_create(&mut self, name: &str) -> Result<()> {
        let url = collections_path(&self.base);
        let body = json!({
            "name": name,
            "get_or_create": true,
            "configuration": {
                "hnsw": {
                    "space": "cosine"
                }
            }
        });
        let resp = self.client.post(&url).json(&body).send()?;
        let resp = Self::check(resp)?;

        #[derive(Deserialize)]
        struct ColResp {
            id: String,
        }
        let col: ColResp = resp.json()?;
        self.collection_id = Some(col.id);
        Ok(())
    }

    /// DELETE /collections/{name} — swallow errors (used by --full-rebuild + GC).
    ///
    /// Deletes by NAME, not id: this ChromaDB rejects DELETE-by-id with 404
    /// (verified against the deployed server), and Python's client deletes by
    /// name too, so by-name is the parity-correct path.
    fn delete_collection(&mut self, name: &str) -> Result<()> {
        let url = format!("{}/{}", collections_path(&self.base), name);
        let _ = self.client.delete(&url).send();
        self.collection_id = None;
        Ok(())
    }

    /// Paginated POST /collections/{id}/get {include:[]} → HashSet of all ids.
    fn existing_ids(&self) -> Result<HashSet<String>> {
        let col_url = self.col_url()?;
        let url = format!("{col_url}/get");
        let mut ids = HashSet::new();
        let mut offset = 0usize;

        loop {
            let body = json!({
                "include": [],
                "limit": PAGE,
                "offset": offset
            });
            let resp = self.client.post(&url).json(&body).send()?;
            let resp = Self::check(resp)?;

            #[derive(Deserialize)]
            struct GetResp {
                ids: Vec<String>,
            }
            let page: GetResp = resp.json()?;
            let n = page.ids.len();
            ids.extend(page.ids);
            if n < PAGE {
                break;
            }
            offset += PAGE;
        }
        Ok(ids)
    }

    /// Paginated POST /collections/{id}/get {include:["metadatas"]} → Vec<(id, Meta)>.
    fn metadatas(&self) -> Result<Vec<(String, Meta)>> {
        let col_url = self.col_url()?;
        let url = format!("{col_url}/get");
        let mut result = Vec::new();
        let mut offset = 0usize;

        loop {
            let body = json!({
                "include": ["metadatas"],
                "limit": PAGE,
                "offset": offset
            });
            let resp = self.client.post(&url).json(&body).send()?;
            let resp = Self::check(resp)?;

            // Response is columnar: {"ids": [...], "metadatas": [{..}|null, ...]}
            #[derive(Deserialize)]
            struct GetResp {
                ids: Vec<String>,
                metadatas: Vec<Option<Value>>,
            }
            let page: GetResp = resp.json()?;
            let n = page.ids.len();
            for (id, maybe_meta) in page.ids.into_iter().zip(page.metadatas) {
                if let Some(m) = maybe_meta {
                    if let Ok(meta) = serde_json::from_value::<Meta>(m) {
                        result.push((id, meta));
                    }
                }
            }
            if n < PAGE {
                break;
            }
            offset += PAGE;
        }
        Ok(result)
    }

    /// POST /collections/{id}/add  {ids, embeddings, documents, metadatas}
    fn add(&mut self, records: &[Record], embeddings: &[Vec<f32>]) -> Result<usize> {
        assert_eq!(
            records.len(),
            embeddings.len(),
            "records and embeddings must have equal length"
        );
        if records.is_empty() {
            return Ok(0);
        }
        let col_url = self.col_url()?;
        let url = format!("{col_url}/add");

        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        let documents: Vec<&str> = records.iter().map(|r| r.body.as_str()).collect();
        let metadatas: Vec<&Meta> = records.iter().map(|r| &r.meta).collect();

        let body = json!({
            "ids": ids,
            "embeddings": embeddings,
            "documents": documents,
            "metadatas": metadatas,
        });

        let resp = self.client.post(&url).json(&body).send()?;
        Self::check(resp)?;
        Ok(records.len())
    }

    /// POST /collections/{id}/delete  {ids: [...]}
    fn delete(&mut self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let col_url = self.col_url()?;
        let url = format!("{col_url}/delete");
        let body = json!({ "ids": ids });
        let resp = self.client.post(&url).json(&body).send()?;
        Self::check(resp)?;
        Ok(ids.len())
    }

    /// GET /collections/{id}/count → bare integer in body (NOT a JSON object).
    fn count(&self) -> Result<usize> {
        let col_url = self.col_url()?;
        let url = format!("{col_url}/count");
        let resp = self.client.get(&url).send()?;
        let resp = Self::check(resp)?;
        let text = resp.text()?;
        Ok(text.trim().parse::<usize>()?)
    }

    /// GET /collections?limit=… → parse id/name/metadata into CollectionInfo.
    fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        let url = format!("{}?limit=100000", collections_path(&self.base));
        let resp = self.client.get(&url).send()?;
        let resp = Self::check(resp)?;
        let v: Value = resp.json()?;
        Ok(parse_collection_list(&v))
    }

    /// PUT /collections/{id} {new_metadata:{index_repo:true,last_indexed:now}}.
    /// Metadata is overwritten wholesale; the hnsw config (separate field) is
    /// untouched.
    fn touch_collection(&mut self, now: u64) -> Result<()> {
        let url = self.col_url()?;
        let body = json!({ "new_metadata": { "index_repo": true, "last_indexed": now } });
        let resp = self.client.put(&url).json(&body).send()?;
        Self::check(resp)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests (pure, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Meta;

    // ---- URL helpers ----

    #[test]
    fn base_url_and_paths() {
        let b = base_url("192.168.1.2", 8000, false);
        assert_eq!(b, "http://192.168.1.2:8000/api/v2");
        assert_eq!(base_url("h", 8000, true), "https://h:8000/api/v2");
    }

    #[test]
    fn collections_path_uses_default_tenant_db() {
        assert_eq!(
            collections_path(&base_url("h", 8000, false)),
            "http://h:8000/api/v2/tenants/default_tenant/databases/default_database/collections"
        );
    }

    // ---- Add-body shape ----

    fn sample_record() -> Record {
        Record {
            id: "abc123".to_string(),
            body: "fn foo() {}".to_string(),
            meta: Meta {
                path: "src/lib.rs".to_string(),
                line: 1,
                lang: "rs".to_string(),
                node_type: "function_item".to_string(),
                scope: "MyMod".to_string(),
            },
        }
    }

    fn sample_embedding() -> Vec<f32> {
        vec![0.1_f32; 384]
    }

    #[test]
    fn add_body_shape() {
        // Build the add JSON body the same way HttpStore::add would and assert its shape.
        let records = vec![sample_record()];
        let embeddings = vec![sample_embedding()];

        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        let documents: Vec<&str> = records.iter().map(|r| r.body.as_str()).collect();
        let metas: Vec<&Meta> = records.iter().map(|r| &r.meta).collect();

        let body = json!({
            "ids": ids,
            "embeddings": embeddings,
            "documents": documents,
            "metadatas": metas,
        });

        // Top-level keys
        assert!(body.get("ids").is_some());
        assert!(body.get("embeddings").is_some());
        assert!(body.get("documents").is_some());
        assert!(body.get("metadatas").is_some());

        // ids is an array
        let ids_arr = body["ids"].as_array().unwrap();
        assert_eq!(ids_arr[0], "abc123");

        // embeddings is a nested array of f32
        let emb_arr = body["embeddings"].as_array().unwrap();
        assert_eq!(emb_arr.len(), 1);
        assert_eq!(emb_arr[0].as_array().unwrap().len(), 384);

        // documents carries the body text
        assert_eq!(body["documents"][0], "fn foo() {}");

        // metadata uses "type" (not "node_type") and includes "scope"
        let meta_v = &body["metadatas"][0];
        assert_eq!(meta_v["type"], "function_item", "must use 'type' key");
        assert!(
            meta_v.get("node_type").is_none(),
            "must NOT have 'node_type' key"
        );
        assert_eq!(meta_v["scope"], "MyMod");
        assert_eq!(meta_v["lang"], "rs");
        assert_eq!(meta_v["line"], 1);

        // metadata without scope must omit the key
        let no_scope_meta = Meta {
            path: "x.py".to_string(),
            line: 5,
            lang: "py".to_string(),
            node_type: "window".to_string(),
            scope: String::new(),
        };
        let v = serde_json::to_value(&no_scope_meta).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("scope"),
            "scope must be absent when empty"
        );
    }

    // ---- col_id guard ----

    #[test]
    fn col_id_errors_without_collection() {
        let store = HttpStore::new("127.0.0.1", 9999, false);
        assert!(
            store.col_id().is_err(),
            "col_id should error before get_or_create"
        );
    }

    // ---- count parses bare integer ----

    #[test]
    fn count_parses_bare_integer() {
        // Simulate what count() does: parse a bare text response
        let text = "42\n";
        let n: usize = text.trim().parse().unwrap();
        assert_eq!(n, 42);

        let text2 = "0";
        let n2: usize = text2.trim().parse().unwrap();
        assert_eq!(n2, 0);
    }

    // ---- pagination body shape ----

    #[test]
    fn existing_ids_body_shape() {
        // Verify the JSON body for a paginated /get call (ids-only)
        let body = json!({
            "include": [],
            "limit": PAGE,
            "offset": 0usize,
        });
        assert_eq!(body["include"], json!([]));
        assert_eq!(body["limit"], PAGE);
        assert_eq!(body["offset"], 0);
    }

    #[test]
    fn metadatas_body_shape() {
        let body = json!({
            "include": ["metadatas"],
            "limit": PAGE,
            "offset": 0usize,
        });
        assert_eq!(body["include"], json!(["metadatas"]));
    }

    // ---- auth header ----

    #[test]
    fn auth_header_value_formats_bearer() {
        let v = auth_header_value("secret-token").expect("valid token");
        assert_eq!(v.to_str().unwrap(), "Bearer secret-token");
    }

    #[test]
    fn auth_header_value_rejects_empty_and_invalid() {
        assert!(auth_header_value("").is_none());
        assert!(auth_header_value("   ").is_none());
        // A newline is an illegal header byte → auth disabled, not a panic.
        assert!(auth_header_value("bad\ntoken").is_none());
    }

    // ---- collection-list parsing ----

    #[test]
    fn parse_collection_list_reads_marker_and_timestamp() {
        // Shape mirrors the deployed ChromaDB v2 list response (verified live).
        let v = json!([
            { "id": "id-a", "name": "code-owner-repo",
              "metadata": { "index_repo": true, "last_indexed": 1_700_000_000u64 } },
            { "id": "id-b", "name": "foreign-col", "metadata": { "hnsw:space": "cosine" } },
            { "id": "id-c", "name": "no-meta", "metadata": null },
            { "id": "id-d", "name": "marked-no-ts", "metadata": { "index_repo": true } }
        ]);
        let got = parse_collection_list(&v);
        assert_eq!(got.len(), 4);
        assert_eq!(
            got[0],
            CollectionInfo {
                id: "id-a".into(),
                name: "code-owner-repo".into(),
                index_repo: true,
                last_indexed: Some(1_700_000_000),
            }
        );
        assert!(!got[1].index_repo && got[1].last_indexed.is_none());
        assert!(!got[2].index_repo && got[2].last_indexed.is_none());
        assert!(got[3].index_repo && got[3].last_indexed.is_none());
    }

    // ---- touch (PUT new_metadata) body shape ----

    #[test]
    fn touch_body_shape() {
        let body =
            json!({ "new_metadata": { "index_repo": true, "last_indexed": 1_700_000_000u64 } });
        assert_eq!(body["new_metadata"]["index_repo"], true);
        assert_eq!(body["new_metadata"]["last_indexed"], 1_700_000_000u64);
    }
}
