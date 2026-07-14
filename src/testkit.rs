use crate::store::{CollectionInfo, Embed, Meta, Record, Store};
use anyhow::Result;
use std::collections::HashSet;

pub(crate) struct MockStore {
    pub ids: HashSet<String>,
    pub metas: Vec<(String, Meta)>,
    pub added: Vec<(Record, Vec<f32>)>,
    pub deleted: Vec<String>,
    pub collection: Option<String>,
    pub fail_add: bool,
    pub collections: Vec<CollectionInfo>,
}

impl MockStore {
    pub(crate) fn new() -> Self {
        Self {
            ids: HashSet::new(),
            metas: Vec::new(),
            added: Vec::new(),
            deleted: Vec::new(),
            collection: None,
            fail_add: false,
            collections: Vec::new(),
        }
    }

    pub(crate) fn with_ids(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.ids.extend(ids);
        self
    }

    pub(crate) fn with_failing_add(mut self) -> Self {
        self.fail_add = true;
        self
    }

    pub(crate) fn with_collections(
        mut self,
        cols: impl IntoIterator<Item = CollectionInfo>,
    ) -> Self {
        self.collections.extend(cols);
        self
    }
}

impl Store for MockStore {
    fn heartbeat(&self) -> Result<()> {
        Ok(())
    }
    fn get_or_create(&mut self, name: &str) -> Result<()> {
        self.collection = Some(name.to_string());
        Ok(())
    }
    fn delete_collection(&mut self, name: &str) -> Result<()> {
        self.collections.retain(|c| c.name != name);
        self.ids.clear();
        self.metas.clear();
        Ok(())
    }
    fn existing_ids(&self) -> Result<HashSet<String>> {
        Ok(self.ids.clone())
    }
    fn metadatas(&self) -> Result<Vec<(String, Meta)>> {
        Ok(self.metas.clone())
    }
    fn add(&mut self, records: &[Record], embeddings: &[Vec<f32>]) -> Result<usize> {
        if self.fail_add {
            anyhow::bail!("simulated add failure");
        }
        for (r, e) in records.iter().zip(embeddings.iter()) {
            self.ids.insert(r.id.clone());
            self.metas.push((r.id.clone(), r.meta.clone()));
            self.added.push((r.clone(), e.clone()));
        }
        Ok(records.len())
    }
    fn delete(&mut self, ids: &[String]) -> Result<usize> {
        let n = ids.len();
        for id in ids {
            self.ids.remove(id);
            self.deleted.push(id.clone());
        }
        Ok(n)
    }
    fn count(&self) -> Result<usize> {
        Ok(self.ids.len())
    }

    fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        Ok(self.collections.clone())
    }

    fn touch_collection(&mut self, now: u64) -> Result<()> {
        if let Some(name) = self.collection.clone() {
            if let Some(c) = self.collections.iter_mut().find(|c| c.name == name) {
                c.index_repo = true;
                c.last_indexed = Some(now);
            }
        }
        Ok(())
    }
}

pub(crate) struct FakeEmbed;

impl Embed for FakeEmbed {
    fn embed(&self, docs: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![vec![0.0_f32; 384]; docs.len()])
    }
}
