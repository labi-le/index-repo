//! Per-root membership manifests, stored in a sidecar ChromaDB collection.
//!
//! The content collection is shared (parity chunk-ids, dedup across checkouts),
//! so a per-root prune must not delete a chunk another root still references.
//! Each root records the id-set it references in its own manifest document(s);
//! it is the sole writer of that manifest, so there is no cross-writer
//! read-modify-write. Orphan collection (`content_ids − union(manifests)`) is a
//! separate single-threaded pass — see `service::gc_orphans`.

use crate::chroma::{base_url, collections_path};
use anyhow::{bail, Result};
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

const PAGE: usize = 10_000;
const EMBED_DIM: usize = 384;

pub trait ManifestStore {
    /// Create or open the sidecar collection.
    fn get_or_create(&mut self) -> Result<()>;

    /// The id-set root `roothash` currently references (latest generation).
    fn read(&self, roothash: &str) -> Result<HashSet<String>>;

    /// Replace root `roothash`'s manifest with `ids`. Writes the new generation
    /// before deleting the old so the manifest union is a monotonic superset for
    /// any concurrent orphan GC — the collection never momentarily under-counts.
    fn write(&mut self, roothash: &str, root: &str, ids: &HashSet<String>) -> Result<()>;

    /// Union of every root's referenced ids — the live set for orphan GC.
    fn all_ids(&self) -> Result<HashSet<String>>;

    /// Drop a root's manifest entirely (its exclusive chunks become orphans).
    fn remove(&mut self, roothash: &str) -> Result<()>;
}

pub struct HttpManifest {
    client: Client,
    base: String,
    name: String,
    collection_id: Option<String>,
}

#[derive(Deserialize)]
struct GetResp {
    ids: Vec<String>,
    #[serde(default, deserialize_with = "null_seq")]
    documents: Vec<Option<String>>,
    #[serde(default, deserialize_with = "null_seq")]
    metadatas: Vec<Option<Value>>,
}

/// ChromaDB `/get` returns an explicit `null` (not an absent field) for
/// `documents`/`metadatas` when they are not requested; accept it as empty.
fn null_seq<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

impl HttpManifest {
    pub fn new(host: &str, port: u16, ssl: bool, name: &str) -> Self {
        let mut builder = Client::builder();
        if let Some(value) = std::env::var("INDEX_REPO_CHROMA_TOKEN")
            .ok()
            .and_then(|t| auth_header_value(&t))
        {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
        Self {
            client: builder.build().expect("failed to build reqwest client"),
            base: base_url(host, port, ssl),
            name: name.to_string(),
            collection_id: None,
        }
    }

    fn col_url(&self) -> Result<String> {
        let id = self
            .collection_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("manifest collection not opened"))?;
        Ok(format!("{}/{}", collections_path(&self.base), id))
    }

    fn check(resp: Response) -> Result<Response> {
        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("HTTP {status}: {body}");
        }
    }

    /// Paginate `/get` with the given request body merged with limit/offset.
    fn get_pages(&self, mut make_body: impl FnMut(usize) -> Value) -> Result<Vec<GetResp>> {
        let url = format!("{}/get", self.col_url()?);
        let mut pages = Vec::new();
        let mut offset = 0usize;
        loop {
            let mut body = make_body(offset);
            body["limit"] = json!(PAGE);
            body["offset"] = json!(offset);
            let resp = Self::check(self.client.post(&url).json(&body).send()?)?;
            let page: GetResp = resp.json()?;
            let n = page.ids.len();
            pages.push(page);
            if n < PAGE {
                break;
            }
            offset += PAGE;
        }
        Ok(pages)
    }

    fn delete_ids(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let url = format!("{}/delete", self.col_url()?);
        Self::check(self.client.post(&url).json(&json!({ "ids": ids })).send()?)?;
        Ok(())
    }
}

fn auth_header_value(token: &str) -> Option<HeaderValue> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    HeaderValue::from_str(&format!("Bearer {token}")).ok()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn meta_u64(meta: &Option<Value>, key: &str) -> Option<u64> {
    meta.as_ref()?.get(key)?.as_u64()
}

fn meta_str<'a>(meta: &'a Option<Value>, key: &str) -> Option<&'a str> {
    meta.as_ref()?.get(key)?.as_str()
}

fn split_ids(body: &str) -> impl Iterator<Item = String> + '_ {
    body.lines().filter(|l| !l.is_empty()).map(str::to_string)
}

impl ManifestStore for HttpManifest {
    fn get_or_create(&mut self) -> Result<()> {
        let url = collections_path(&self.base);
        let body = json!({
            "name": self.name,
            "get_or_create": true,
            "configuration": { "hnsw": { "space": "cosine" } }
        });
        let resp = Self::check(self.client.post(&url).json(&body).send()?)?;
        #[derive(Deserialize)]
        struct ColResp {
            id: String,
        }
        let col: ColResp = resp.json()?;
        self.collection_id = Some(col.id);
        Ok(())
    }

    fn read(&self, roothash: &str) -> Result<HashSet<String>> {
        let pages = self.get_pages(
            |_| json!({ "where": { "roothash": roothash }, "include": ["documents", "metadatas"] }),
        )?;
        let mut latest_gen: u64 = 0;
        let mut by_gen: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        for page in pages {
            for (doc, meta) in page.documents.into_iter().zip(page.metadatas) {
                let gen = meta_u64(&meta, "gen").unwrap_or(0);
                latest_gen = latest_gen.max(gen);
                if let Some(d) = doc {
                    by_gen.entry(gen).or_default().push(d);
                }
            }
        }
        let mut ids = HashSet::new();
        if let Some(bodies) = by_gen.get(&latest_gen) {
            for b in bodies {
                ids.extend(split_ids(b));
            }
        }
        Ok(ids)
    }

    fn write(&mut self, roothash: &str, root: &str, ids: &HashSet<String>) -> Result<()> {
        let gen = now_millis();
        let existing: Vec<String> = self
            .get_pages(|_| json!({ "where": { "roothash": roothash }, "include": [] }))?
            .into_iter()
            .flat_map(|p| p.ids)
            .collect();

        let ordered: Vec<&String> = ids.iter().collect();
        let embedding = {
            let mut v = vec![0.0f32; EMBED_DIM];
            v[0] = 1.0;
            v
        };
        let mut new_ids: Vec<String> = Vec::new();
        let mut docs: Vec<String> = Vec::new();
        let mut metas: Vec<Value> = Vec::new();
        for (part, chunk) in ordered
            .chunks(crate::config::MANIFEST_PART_SIZE)
            .enumerate()
        {
            let body: String = chunk
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            new_ids.push(format!("manifest-{roothash}-{gen}-{part}"));
            docs.push(body);
            metas.push(json!({ "roothash": roothash, "root": root, "gen": gen, "part": part }));
        }

        if !new_ids.is_empty() {
            let embeddings: Vec<&Vec<f32>> =
                std::iter::repeat_n(&embedding, new_ids.len()).collect();
            let url = format!("{}/add", self.col_url()?);
            let add = json!({
                "ids": new_ids,
                "embeddings": embeddings,
                "documents": docs,
                "metadatas": metas,
            });
            Self::check(self.client.post(&url).json(&add).send()?)?;
        }

        let stale: Vec<String> = existing
            .into_iter()
            .filter(|id| !new_ids.contains(id))
            .collect();
        self.delete_ids(&stale)?;
        Ok(())
    }

    fn all_ids(&self) -> Result<HashSet<String>> {
        let pages = self.get_pages(|_| json!({ "include": ["documents", "metadatas"] }))?;
        let mut latest: HashMap<String, u64> = HashMap::new();
        let mut bodies: HashMap<(String, u64), Vec<String>> = HashMap::new();
        for page in pages {
            for (doc, meta) in page.documents.into_iter().zip(page.metadatas) {
                let roothash = meta_str(&meta, "roothash").unwrap_or("").to_string();
                let gen = meta_u64(&meta, "gen").unwrap_or(0);
                let e = latest.entry(roothash.clone()).or_insert(0);
                *e = (*e).max(gen);
                if let Some(d) = doc {
                    bodies.entry((roothash, gen)).or_default().push(d);
                }
            }
        }
        let mut ids = HashSet::new();
        for (roothash, gen) in latest {
            if let Some(bs) = bodies.get(&(roothash, gen)) {
                for b in bs {
                    ids.extend(split_ids(b));
                }
            }
        }
        Ok(ids)
    }

    fn remove(&mut self, roothash: &str) -> Result<()> {
        let ids: Vec<String> = self
            .get_pages(|_| json!({ "where": { "roothash": roothash }, "include": [] }))?
            .into_iter()
            .flat_map(|p| p.ids)
            .collect();
        self.delete_ids(&ids)
    }
}
