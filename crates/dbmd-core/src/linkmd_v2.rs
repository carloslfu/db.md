// SPDX-License-Identifier: Apache-2.0

//! Pure link.md wire-profile-v2 primitives.
//!
//! This module is deterministic protocol plumbing: portable paths, canonical
//! domain-separated hashing, the bounded 16-way content HAMT, and hiding-proof
//! verification. It performs no network I/O and contains no model dependency.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub const MAX_PATH_BYTES: usize = 1_024;
pub const MAX_COMPONENT_BYTES: usize = 255;
const NODE_DOMAIN: &str = "v2/content-tree-node";

#[derive(Debug, thiserror::Error)]
pub enum V2Error {
    #[error("invalid portable path: {0}")]
    InvalidPath(String),
    #[error("invalid v2 tree: {0}")]
    InvalidTree(String),
    #[error("missing v2 tree object {0}")]
    MissingNode(String),
}

pub type V2Result<T> = Result<T, V2Error>;

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn canonical_bytes(value: &Value) -> V2Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| V2Error::InvalidTree(format!("canonical JSON failed: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn domain_hash_bytes(domain: &str, bytes: &[u8]) -> V2Result<String> {
    if domain.is_empty()
        || domain.len() > 128
        || !domain.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'/' | b'-'))
        })
    {
        return Err(V2Error::InvalidTree("invalid hash domain".to_string()));
    }
    let mut hasher = Sha256::new();
    hasher.update(b"link.md\0");
    hasher.update(domain.as_bytes());
    hasher.update(b"\0");
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn domain_hash(domain: &str, value: &Value) -> V2Result<String> {
    if domain.is_empty()
        || domain.len() > 128
        || !domain.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'/' | b'-'))
        })
    {
        return Err(V2Error::InvalidTree("invalid hash domain".to_string()));
    }
    let mut hasher = Sha256::new();
    hasher.update(b"link.md\0");
    hasher.update(domain.as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_bytes(value)?);
    Ok(format!("{:x}", hasher.finalize()))
}

fn portable_alias(component: &str) -> String {
    component
        .nfc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .nfc()
        .collect()
}

fn windows_device(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .to_ascii_lowercase();
    matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || stem
            .strip_prefix("com")
            .or_else(|| stem.strip_prefix("lpt"))
            .is_some_and(|digit| digit.len() == 1 && matches!(digit.as_bytes()[0], b'1'..=b'9'))
}

pub fn normalize_path(input: &str) -> V2Result<String> {
    if input.is_empty()
        || input.len() > MAX_PATH_BYTES
        || input.starts_with('/')
        || input.contains(['\\', ':', '\0'])
        || input.nfc().collect::<String>() != input
    {
        return Err(V2Error::InvalidPath(input.to_string()));
    }
    for component in input.split('/') {
        let alias = portable_alias(component);
        if component.is_empty()
            || matches!(component, "." | "..")
            || component.ends_with(['.', ' '])
            || component.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
            || component.len() > MAX_COMPONENT_BYTES
            || alias.len() > MAX_COMPONENT_BYTES
            || windows_device(component)
        {
            return Err(V2Error::InvalidPath(input.to_string()));
        }
    }
    Ok(input.to_string())
}

pub fn validate_path_set<'a>(paths: impl IntoIterator<Item = &'a str>) -> V2Result<Vec<String>> {
    let normalized = paths
        .into_iter()
        .map(normalize_path)
        .collect::<V2Result<Vec<_>>>()?;
    let mut exact = BTreeSet::new();
    let mut aliases = BTreeMap::new();
    for path in &normalized {
        if !exact.insert(path.clone()) {
            return Err(V2Error::InvalidPath(format!("duplicate path: {path}")));
        }
        let alias = path
            .split('/')
            .map(portable_alias)
            .collect::<Vec<_>>()
            .join("/");
        if let Some(prior) = aliases.insert(alias, path.clone()) {
            if prior != *path {
                return Err(V2Error::InvalidPath(format!(
                    "portable alias collision: {prior} and {path}"
                )));
            }
        }
    }
    for path in &normalized {
        let components = path.split('/').collect::<Vec<_>>();
        for index in 1..components.len() {
            let prefix = components[..index].join("/");
            if exact.contains(&prefix) {
                return Err(V2Error::InvalidPath(format!(
                    "file/directory prefix collision: {prefix}"
                )));
            }
        }
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Blob,
    Tree,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub child_hash: String,
    pub bytes: Option<u64>,
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HamtNode {
    Leaf {
        route: String,
        entry: TreeEntry,
    },
    Branch {
        depth: usize,
        children: Vec<(u8, String)>,
    },
    Compressed {
        depth: usize,
        run: String,
        child: String,
    },
}

fn node_value(node: &HamtNode) -> Value {
    match node {
        HamtNode::Leaf { route, entry } => json!({
            "entry": {
                "bytes": entry.bytes,
                "child_hash": entry.child_hash,
                "kind": entry.kind,
                "name": entry.name,
                "nonce": entry.nonce,
            },
            "kind": "leaf",
            "route": route,
            "v": 1,
        }),
        HamtNode::Branch { depth, children } => json!({
            "children": children,
            "depth": depth,
            "kind": "branch",
            "v": 1,
        }),
        HamtNode::Compressed { depth, run, child } => json!({
            "child": child,
            "depth": depth,
            "kind": "compressed",
            "run": run,
            "v": 1,
        }),
    }
}

pub fn encode_node(node: &HamtNode) -> V2Result<Vec<u8>> {
    canonical_bytes(&node_value(node))
}

pub fn hash_node(node: &HamtNode) -> V2Result<String> {
    domain_hash(NODE_DOMAIN, &node_value(node))
}

pub fn decode_node(bytes: &[u8]) -> V2Result<HamtNode> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| V2Error::InvalidTree(format!("node JSON failed: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| V2Error::InvalidTree("node is not an object".to_string()))?;
    if object.get("v").and_then(Value::as_u64) != Some(1) {
        return Err(V2Error::InvalidTree("unsupported node version".to_string()));
    }
    let node = match object.get("kind").and_then(Value::as_str) {
        Some("leaf") => {
            let route = object
                .get("route")
                .and_then(Value::as_str)
                .ok_or_else(|| V2Error::InvalidTree("leaf route missing".to_string()))?;
            let entry_value = object
                .get("entry")
                .ok_or_else(|| V2Error::InvalidTree("leaf entry missing".to_string()))?;
            let entry: TreeEntry = serde_json::from_value(entry_value.clone())
                .map_err(|error| V2Error::InvalidTree(format!("leaf entry failed: {error}")))?;
            HamtNode::Leaf {
                route: route.to_string(),
                entry,
            }
        }
        Some("branch") => {
            let depth = object
                .get("depth")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| V2Error::InvalidTree("branch depth missing".to_string()))?;
            let children = serde_json::from_value(
                object
                    .get("children")
                    .cloned()
                    .ok_or_else(|| V2Error::InvalidTree("branch children missing".to_string()))?,
            )
            .map_err(|error| V2Error::InvalidTree(format!("branch children failed: {error}")))?;
            HamtNode::Branch { depth, children }
        }
        Some("compressed") => HamtNode::Compressed {
            depth: object
                .get("depth")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| V2Error::InvalidTree("compressed depth missing".to_string()))?,
            run: object
                .get("run")
                .and_then(Value::as_str)
                .ok_or_else(|| V2Error::InvalidTree("compressed run missing".to_string()))?
                .to_string(),
            child: object
                .get("child")
                .and_then(Value::as_str)
                .ok_or_else(|| V2Error::InvalidTree("compressed child missing".to_string()))?
                .to_string(),
        },
        _ => return Err(V2Error::InvalidTree("unknown node kind".to_string())),
    };
    if encode_node(&node)? != bytes {
        return Err(V2Error::InvalidTree("non-canonical node".to_string()));
    }
    Ok(node)
}

fn put_node(nodes: &mut BTreeMap<String, Vec<u8>>, node: HamtNode) -> V2Result<String> {
    let hash = hash_node(&node)?;
    let bytes = encode_node(&node)?;
    if let Some(prior) = nodes.get(&hash) {
        if prior != &bytes {
            return Err(V2Error::InvalidTree("node hash collision".to_string()));
        }
    }
    nodes.insert(hash.clone(), bytes);
    Ok(hash)
}

fn common_run(routes: &[String], depth: usize) -> String {
    if routes.len() == 1 {
        return routes[0][depth..].to_string();
    }
    let mut end = depth;
    while end < 64 {
        let byte = routes[0].as_bytes()[end];
        if routes.iter().any(|route| route.as_bytes()[end] != byte) {
            break;
        }
        end += 1;
    }
    routes[0][depth..end].to_string()
}

fn build_node(
    leaves: &[(String, TreeEntry)],
    depth: usize,
    nodes: &mut BTreeMap<String, Vec<u8>>,
) -> V2Result<String> {
    if leaves.is_empty() || depth > 64 {
        return Err(V2Error::InvalidTree(
            "invalid HAMT build bounds".to_string(),
        ));
    }
    if leaves.len() == 1 {
        let leaf_hash = put_node(
            nodes,
            HamtNode::Leaf {
                route: leaves[0].0.clone(),
                entry: leaves[0].1.clone(),
            },
        )?;
        let run = leaves[0].0[depth..].to_string();
        return if run.is_empty() {
            Ok(leaf_hash)
        } else {
            put_node(
                nodes,
                HamtNode::Compressed {
                    depth,
                    run,
                    child: leaf_hash,
                },
            )
        };
    }
    let run = common_run(
        &leaves
            .iter()
            .map(|(route, _)| route.clone())
            .collect::<Vec<_>>(),
        depth,
    );
    if !run.is_empty() {
        let child = build_node(leaves, depth + run.len(), nodes)?;
        return put_node(nodes, HamtNode::Compressed { depth, run, child });
    }
    if depth >= 64 {
        return Err(V2Error::InvalidTree(
            "distinct names have a SHA-256 route collision".to_string(),
        ));
    }
    let mut groups: BTreeMap<u8, Vec<(String, TreeEntry)>> = BTreeMap::new();
    for leaf in leaves {
        let slot = u8::from_str_radix(&leaf.0[depth..=depth], 16)
            .map_err(|_| V2Error::InvalidTree("invalid route nibble".to_string()))?;
        groups.entry(slot).or_default().push(leaf.clone());
    }
    let mut children = Vec::new();
    for (slot, group) in groups {
        children.push((slot, build_node(&group, depth + 1, nodes)?));
    }
    put_node(nodes, HamtNode::Branch { depth, children })
}

pub fn build_hamt(
    entries: &[TreeEntry],
    nodes: &mut BTreeMap<String, Vec<u8>>,
) -> V2Result<Option<String>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut names = BTreeSet::new();
    let mut routes = BTreeSet::new();
    let mut leaves = Vec::new();
    for entry in entries {
        if entry.name.nfc().collect::<String>() != entry.name
            || entry.nonce.len() != 32
            || !entry
                .nonce
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(V2Error::InvalidTree("invalid leaf state".to_string()));
        }
        if !names.insert(entry.name.clone()) {
            return Err(V2Error::InvalidTree("duplicate child name".to_string()));
        }
        let route = sha256_hex(entry.name.as_bytes());
        if !routes.insert(route.clone()) {
            return Err(V2Error::InvalidTree(
                "child-name route collision".to_string(),
            ));
        }
        leaves.push((route, entry.clone()));
    }
    leaves.sort_by(|left, right| left.0.cmp(&right.0));
    build_node(&leaves, 0, nodes).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentFile {
    pub path: String,
    pub blob_hash: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryState {
    pub path: String,
    pub entry: TreeEntry,
}

#[derive(Debug, Clone)]
pub struct BuiltTree {
    pub root: Option<String>,
    pub nodes: BTreeMap<String, Vec<u8>>,
    pub entries: BTreeMap<String, EntryState>,
    pub files: BTreeMap<String, ContentFile>,
}

#[derive(Default)]
struct Directory {
    files: BTreeMap<String, ContentFile>,
    dirs: BTreeMap<String, Directory>,
}

fn next_nonce(factory: &mut impl FnMut() -> String, prior: Option<&str>) -> V2Result<String> {
    for _ in 0..8 {
        let nonce = factory();
        if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(V2Error::InvalidTree(
                "nonce factory returned invalid bytes".to_string(),
            ));
        }
        if Some(nonce.as_str()) != prior {
            return Ok(nonce);
        }
    }
    Err(V2Error::InvalidTree(
        "nonce factory repeated the prior nonce".to_string(),
    ))
}

pub fn build_content_tree(
    input: &[ContentFile],
    prior: Option<&BuiltTree>,
    nonce_factory: &mut impl FnMut() -> String,
) -> V2Result<BuiltTree> {
    let paths = validate_path_set(input.iter().map(|file| file.path.as_str()))?;
    let mut directory = Directory::default();
    let mut files = BTreeMap::new();
    for (index, file) in input.iter().enumerate() {
        if file.blob_hash.len() != 64
            || !file.blob_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(V2Error::InvalidTree("invalid blob hash".to_string()));
        }
        let normalized = ContentFile {
            path: paths[index].clone(),
            blob_hash: file.blob_hash.clone(),
            bytes: file.bytes,
        };
        files.insert(normalized.path.clone(), normalized.clone());
        let components = normalized.path.split('/').collect::<Vec<_>>();
        let mut current = &mut directory;
        for component in &components[..components.len() - 1] {
            current = current.dirs.entry((*component).to_string()).or_default();
        }
        current.files.insert(
            components.last().expect("path has a component").to_string(),
            normalized,
        );
    }

    fn recurse(
        directory: &Directory,
        prefix: &str,
        prior: Option<&BuiltTree>,
        nonce_factory: &mut impl FnMut() -> String,
        nodes: &mut BTreeMap<String, Vec<u8>>,
        states: &mut BTreeMap<String, EntryState>,
    ) -> V2Result<Option<String>> {
        let mut entries = Vec::new();
        for (name, child) in &directory.dirs {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let Some(child_hash) = recurse(child, &path, prior, nonce_factory, nodes, states)?
            else {
                continue;
            };
            let old = prior.and_then(|tree| tree.entries.get(&path));
            let unchanged = old.is_some_and(|state| {
                state.entry.name == *name
                    && state.entry.kind == EntryKind::Tree
                    && state.entry.child_hash == child_hash
                    && state.entry.bytes.is_none()
            });
            let nonce = if unchanged {
                old.expect("checked").entry.nonce.clone()
            } else {
                next_nonce(nonce_factory, old.map(|state| state.entry.nonce.as_str()))?
            };
            let entry = TreeEntry {
                name: name.clone(),
                kind: EntryKind::Tree,
                child_hash,
                bytes: None,
                nonce,
            };
            states.insert(
                path.clone(),
                EntryState {
                    path,
                    entry: entry.clone(),
                },
            );
            entries.push(entry);
        }
        for (name, file) in &directory.files {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let old = prior.and_then(|tree| tree.entries.get(&path));
            let unchanged = old.is_some_and(|state| {
                state.entry.name == *name
                    && state.entry.kind == EntryKind::Blob
                    && state.entry.child_hash == file.blob_hash
                    && state.entry.bytes == Some(file.bytes)
            });
            let nonce = if unchanged {
                old.expect("checked").entry.nonce.clone()
            } else {
                next_nonce(nonce_factory, old.map(|state| state.entry.nonce.as_str()))?
            };
            let entry = TreeEntry {
                name: name.clone(),
                kind: EntryKind::Blob,
                child_hash: file.blob_hash.clone(),
                bytes: Some(file.bytes),
                nonce,
            };
            states.insert(
                path.clone(),
                EntryState {
                    path,
                    entry: entry.clone(),
                },
            );
            entries.push(entry);
        }
        build_hamt(&entries, nodes)
    }

    let mut nodes = BTreeMap::new();
    let mut entries = BTreeMap::new();
    let root = recurse(
        &directory,
        "",
        prior,
        nonce_factory,
        &mut nodes,
        &mut entries,
    )?;
    Ok(BuiltTree {
        root,
        nodes,
        entries,
        files,
    })
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProofFrame {
    Branch {
        depth: usize,
        slot: u8,
        siblings: Vec<(u8, String)>,
    },
    Compressed {
        depth: usize,
        run: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NonInclusionTerminal {
    EmptyBranch {
        depth: usize,
        slot: u8,
        siblings: Vec<(u8, String)>,
    },
    CompressedMismatch {
        depth: usize,
        run: String,
        child: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HamtProof {
    Inclusion {
        entry: TreeEntry,
        route: String,
        frames: Vec<ProofFrame>,
    },
    NonInclusion {
        name: String,
        route: String,
        terminal: NonInclusionTerminal,
        frames: Vec<ProofFrame>,
    },
}

pub fn create_proof(
    root: &str,
    name: &str,
    nodes: &BTreeMap<String, Vec<u8>>,
) -> V2Result<HamtProof> {
    let route = sha256_hex(name.nfc().collect::<String>().as_bytes());
    let mut frames = Vec::new();
    let mut hash = root.to_string();
    loop {
        let bytes = nodes
            .get(&hash)
            .ok_or_else(|| V2Error::MissingNode(hash.clone()))?;
        let node = decode_node(bytes)?;
        if hash_node(&node)? != hash {
            return Err(V2Error::InvalidTree("node address mismatch".to_string()));
        }
        match node {
            HamtNode::Leaf {
                route: leaf_route,
                entry,
            } => {
                if leaf_route != route || entry.name != name {
                    return Err(V2Error::InvalidTree(
                        "cryptographic name-route collision".to_string(),
                    ));
                }
                return Ok(HamtProof::Inclusion {
                    entry,
                    route,
                    frames,
                });
            }
            HamtNode::Compressed { depth, run, child } => {
                if route[depth..depth + run.len()] != run {
                    return Ok(HamtProof::NonInclusion {
                        name: name.to_string(),
                        route,
                        terminal: NonInclusionTerminal::CompressedMismatch { depth, run, child },
                        frames,
                    });
                }
                frames.push(ProofFrame::Compressed {
                    depth,
                    run: run.clone(),
                });
                hash = child;
            }
            HamtNode::Branch { depth, children } => {
                let slot = u8::from_str_radix(&route[depth..=depth], 16)
                    .map_err(|_| V2Error::InvalidTree("invalid route nibble".to_string()))?;
                let child = children.iter().find(|(candidate, _)| *candidate == slot);
                let siblings = children
                    .iter()
                    .filter(|(candidate, _)| *candidate != slot)
                    .cloned()
                    .collect::<Vec<_>>();
                let Some((_, child_hash)) = child else {
                    return Ok(HamtProof::NonInclusion {
                        name: name.to_string(),
                        route,
                        terminal: NonInclusionTerminal::EmptyBranch {
                            depth,
                            slot,
                            siblings,
                        },
                        frames,
                    });
                };
                frames.push(ProofFrame::Branch {
                    depth,
                    slot,
                    siblings,
                });
                hash = child_hash.clone();
            }
        }
    }
}

pub fn verify_proof(root: &str, name: &str, proof: &HamtProof) -> V2Result<bool> {
    let normalized = name.nfc().collect::<String>();
    let route = sha256_hex(normalized.as_bytes());
    let (mut current, frames) = match proof {
        HamtProof::Inclusion {
            entry,
            route: proof_route,
            frames,
        } => {
            if proof_route != &route || entry.name != normalized {
                return Ok(false);
            }
            (
                hash_node(&HamtNode::Leaf {
                    route: route.clone(),
                    entry: entry.clone(),
                })?,
                frames,
            )
        }
        HamtProof::NonInclusion {
            route: proof_route,
            terminal,
            frames,
            ..
        } => {
            if proof_route != &route {
                return Ok(false);
            }
            let hash = match terminal {
                NonInclusionTerminal::CompressedMismatch { depth, run, child } => {
                    if route[*depth..*depth + run.len()] == *run {
                        return Ok(false);
                    }
                    hash_node(&HamtNode::Compressed {
                        depth: *depth,
                        run: run.clone(),
                        child: child.clone(),
                    })?
                }
                NonInclusionTerminal::EmptyBranch {
                    depth,
                    slot,
                    siblings,
                } => {
                    let wanted = u8::from_str_radix(&route[*depth..=*depth], 16)
                        .map_err(|_| V2Error::InvalidTree("invalid route nibble".to_string()))?;
                    if wanted != *slot || siblings.iter().any(|(candidate, _)| candidate == slot) {
                        return Ok(false);
                    }
                    hash_node(&HamtNode::Branch {
                        depth: *depth,
                        children: siblings.clone(),
                    })?
                }
            };
            (hash, frames)
        }
    };
    for frame in frames.iter().rev() {
        current = match frame {
            ProofFrame::Compressed { depth, run } => hash_node(&HamtNode::Compressed {
                depth: *depth,
                run: run.clone(),
                child: current,
            })?,
            ProofFrame::Branch {
                depth,
                slot,
                siblings,
            } => {
                let mut children = siblings.clone();
                children.push((*slot, current));
                children.sort_by_key(|(candidate, _)| *candidate);
                if children.windows(2).any(|window| window[0].0 == window[1].0) {
                    return Ok(false);
                }
                hash_node(&HamtNode::Branch {
                    depth: *depth,
                    children,
                })?
            }
        }
    }
    Ok(current == root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce_sequence() -> impl FnMut() -> String {
        let mut value = 0u128;
        move || {
            let nonce = format!("{value:032x}");
            value += 1;
            nonce
        }
    }

    fn file(path: &str, bytes: &[u8]) -> ContentFile {
        ContentFile {
            path: path.to_string(),
            blob_hash: sha256_hex(bytes),
            bytes: bytes.len() as u64,
        }
    }

    #[test]
    fn canonical_tree_and_proofs() {
        let mut nonces = nonce_sequence();
        let tree = build_content_tree(
            &[
                file("DB.md", b"db"),
                file("secret.md", b"secret"),
                file("visible.md", b"visible"),
            ],
            None,
            &mut nonces,
        )
        .unwrap();
        let root = tree.root.as_deref().unwrap();
        assert_eq!(
            root,
            "82bcc02847453aac64310ad0ab83a2cce3f79ec7bac7664f0e6760cb38bc3d53"
        );
        let proof = create_proof(root, "visible.md", &tree.nodes).unwrap();
        assert!(verify_proof(root, "visible.md", &proof).unwrap());
        let encoded = serde_json::to_string(&proof).unwrap();
        assert!(!encoded.contains("secret.md"));
        assert!(!encoded.contains(&tree.entries["secret.md"].entry.nonce));
        let missing = create_proof(root, "missing.md", &tree.nodes).unwrap();
        assert!(verify_proof(root, "missing.md", &missing).unwrap());
    }

    #[test]
    fn rejects_portability_collisions() {
        assert!(validate_path_set(["Records/a.md", "records/a.md"]).is_err());
        assert!(validate_path_set(["records", "records/a.md"]).is_err());
        assert!(normalize_path("CON").is_err());
    }
}
