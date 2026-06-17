use index_repo::embed::Embedder;
use index_repo::store::Embed;
use std::fs;

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump <docs.json>");
    let docs: Vec<String> = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let embedder = Embedder::from_env().unwrap();
    let vecs = embedder.embed(&docs).unwrap();
    println!("{}", serde_json::to_string(&vecs).unwrap());
}
