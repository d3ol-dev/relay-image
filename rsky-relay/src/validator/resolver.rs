use std::io::BufRead;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use hashbrown::HashSet;
use lru::LruCache;
use reqwest::Client;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::value::RawValue;
use thiserror::Error;
use tokio::time::timeout;

use rsky_identity::types::DidDocument;

use crate::config::{CAPACITY_CACHE, DO_PLC_EXPORT, PLC_EXPORT_INTERVAL};
use crate::validator::event::{DidEndpoint, DidKey};

const POLL_TIMEOUT: Duration = Duration::from_micros(10);
const REQ_TIMEOUT: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE: Duration = Duration::from_secs(300);

const PLC_URL: &str = "https://plc.directory";
const PLC_EXPORT: &str = "export?count=1000&after";
const DOC_PATH: &str = ".well-known/did.json";

type RequestFuture = Pin<Box<dyn Future<Output = (Query, reqwest::Result<Bytes>)> + Send>>;

#[derive(Debug)]
enum Query {
    Did(String),
    Export(String),
}

#[derive(Debug, Error)]
pub enum ResolverError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("size error")]
    SizeError,
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

pub struct Resolver {
    cache: LruCache<String, (DidEndpoint, DidKey)>,
    conn: Connection,
    last: Instant,
    after: Option<String>,
    client: Client,
    inflight: HashSet<String>,
    futures: FuturesUnordered<RequestFuture>,
}

impl Resolver {
    pub fn new() -> Result<Self, ResolverError> {
        #[expect(clippy::unwrap_used)]
        let cache = LruCache::new(NonZeroUsize::new(CAPACITY_CACHE).unwrap());
        let flag = if *DO_PLC_EXPORT {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let conn = Connection::open_with_flags(
            "plc_directory.db",
            flag | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        if *DO_PLC_EXPORT {
            match conn.execute("PRAGMA secure_delete = OFF", []) {
                Ok(_) | Err(rusqlite::Error::ExecuteReturnedResults) => {}
                Err(err) => Err(err)?,
            };
            conn.execute("PRAGMA synchronous = NORMAL", [])?;
            conn.execute("PRAGMA incremental_vacuum", [])?;
            conn.execute("PRAGMA optimize = 0x10002", [])?;
        }
        let now = Instant::now();
        let last = now.checked_sub(PLC_EXPORT_INTERVAL).unwrap_or(now);
        let after = conn.query_one(
            "SELECT created_at FROM plc_operations ORDER BY created_at DESC LIMIT 1",
            [],
            |row| Ok(Some(row.get("created_at")?)),
        )?;
        let client = Client::builder()
            .user_agent("rsky-relay")
            .timeout(REQ_TIMEOUT)
            .tcp_keepalive(Some(TCP_KEEPALIVE))
            .https_only(true)
            .build()?;
        let inflight = HashSet::new();
        let futures = FuturesUnordered::new();
        Ok(Self { cache, conn, last, after, client, inflight, futures })
    }

    pub fn expire(&mut self, did: &str, time: DateTime<Utc>) {
        if let Some(after) = &self.after {
            if DateTime::parse_from_rfc3339(after).map_or(true, |after| after < time) {
                tracing::trace!("expiring did");
                self.cache.pop(did);
                self.request(did);
            }
        }
    }

    pub fn resolve(&mut self, did: &str) -> Result<Option<(Option<&str>, &DidKey)>, ResolverError> {
        // the identity might have expired, so check inflight dids first
        if self.inflight.contains(did) {
            return Ok(None);
        }
        // if let Some(_) = self.cache.get(did) doesn't work because of NLL
        if self.cache.get(did).is_some() || self.query_db(did)? {
            return Ok(self.cache.peek_mru().map(|(_, v)| (v.0.as_ref().map(AsRef::as_ref), &v.1)));
        }
        self.request(did);
        Ok(None)
    }

    pub fn query_db(&mut self, did: &str) -> Result<bool, ResolverError> {
        let mut stmt = self.conn.prepare_cached("SELECT * FROM plc_keys WHERE did = ?1")?;
        match stmt.query_one([did], |row| {
            let endpoint =
                if cfg!(feature = "labeler") { "labeler_endpoint" } else { "pds_endpoint" };
            let key = if cfg!(feature = "labeler") { "labeler_key" } else { "pds_key" };
            let endpoint = row.get_ref(endpoint)?.as_str_or_null()?;
            let key = row.get_ref(key)?.as_str_or_null()?;
            Ok(parse_key_endpoint(endpoint, key))
        }) {
            Ok(Some((pds, key))) => {
                self.cache.put(did.to_owned(), (pds, key));
                return Ok(true);
            }
            Ok(None) => {}
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                tracing::trace!("not found in db");
            }
            Err(err) => Err(err)?,
        }
        drop(stmt);
        Ok(false)
    }

    pub fn request(&mut self, did: &str) {
        self.inflight.insert(did.to_owned());
        if let Some(plc) = did.strip_prefix("did:plc:") {
            let plc = if *DO_PLC_EXPORT { None } else { Some(plc) };
            self.send_req(None, plc);
        } else if let Some(web) = did.strip_prefix("did:web:") {
            let Ok(web) = urlencoding::decode(web) else {
                tracing::debug!(%did, "invalid did");
                return;
            };
            self.send_req(Some(&web), None);
        } else {
            tracing::debug!(%did, "invalid did");
            self.inflight.remove(did);
        }
    }

    fn send_req(&mut self, web: Option<&str>, plc: Option<&str>) {
        let (req, query) = if let Some(web) = web {
            tracing::trace!("fetching did");
            (self.client.get(format!("https://{web}/{DOC_PATH}")), Query::Did(web.to_owned()))
        } else if let Some(plc) = plc {
            tracing::trace!("fetching did");
            (self.client.get(format!("{PLC_URL}/did:plc:{plc}")), Query::Did(plc.to_owned()))
        } else if let Some(after) = self.after.take() {
            tracing::trace!(%after, "fetching after");
            self.last = Instant::now();
            (self.client.get(format!("{PLC_URL}/{PLC_EXPORT}={after}")), Query::Export(after))
        } else {
            return;
        };
        self.futures.push(Box::pin(async move {
            match req.send().await {
                Ok(req) => match req.bytes().await {
                    Ok(bytes) => (query, Ok(bytes)),
                    Err(err) => (query, Err(err)),
                },
                Err(err) => (query, Err(err)),
            }
        }));
    }

    pub async fn poll(&mut self) -> Result<Vec<String>, ResolverError> {
        if let Ok(Some((query, res))) = timeout(POLL_TIMEOUT, self.futures.next()).await {
            match res {
                Ok(bytes) => match query {
                    Query::Did(query) => {
                        if let Some((did, (pds, key))) = parse_did_doc(&bytes) {
                            if query != did[8..] {
                                tracing::warn!(%query, found = %&did[8..], "did query mismatch");
                                return Ok(Vec::new());
                            }
                            self.inflight.remove(&did);
                            self.cache.put(did.clone(), (pds, key));
                            return Ok(vec![did]);
                        }
                    }
                    Query::Export(after) => {
                        self.after = Some(after);
                        let mut dids = Vec::new();
                        let mut count = 0;
                        let tx = self.conn.transaction()?;
                        let mut stmt = tx.prepare_cached("INSERT INTO plc_operations (cid, did, created_at, nullified, operation) VALUES (?1, ?2, ?3, ?4, ?5)")?;
                        for line in bytes.reader().lines() {
                            count += 1;
                            if let Some(doc) = parse_plc_doc(&line.unwrap_or_default()) {
                                stmt.execute((
                                    &doc.cid,
                                    &doc.did,
                                    &doc.created_at,
                                    &doc.nullified,
                                    doc.operation.get().as_bytes(),
                                ))?;
                                self.after = Some(doc.created_at);
                                if self.inflight.remove(&doc.did) {
                                    dids.push(doc.did);
                                }
                            }
                        }
                        drop(stmt);
                        tx.commit()?;
                        if count == 1000 {
                            self.send_req(None, None);
                        } else {
                            // no more plc operations, drain inflight dids
                            dids.extend(
                                self.inflight.extract_if(|did| did.starts_with("did:plc:")),
                            );
                        }
                        return Ok(dids);
                    }
                },
                Err(err) => {
                    tracing::debug!(%err, "fetch error");
                }
            }
        } else if *DO_PLC_EXPORT && self.last.elapsed() > PLC_EXPORT_INTERVAL {
            self.send_req(None, None);
        }
        Ok(Vec::new())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlcDocument<'a> {
    did: String,
    #[serde(borrow)]
    operation: &'a RawValue,
    cid: String,
    nullified: bool,
    created_at: String,
}

fn parse_plc_doc(input: &str) -> Option<PlcDocument<'_>> {
    match serde_json::from_slice::<PlcDocument<'_>>(input.as_bytes()) {
        Ok(doc) => {
            return Some(doc);
        }
        Err(err) => {
            tracing::debug!(%input, %err, "parse error");
        }
    }
    None
}

fn parse_did_doc(input: &Bytes) -> Option<(String, (DidEndpoint, DidKey))> {
    match serde_json::from_slice::<DidDocument>(input) {
        Ok(doc) => {
            let endpoint =
                if cfg!(feature = "labeler") { "#atproto_labeler" } else { "#atproto_pds" };
            let key = if cfg!(feature = "labeler") { "#atproto_label" } else { "#atproto" };
            let endpoint = doc
                .service
                .as_ref()
                .and_then(|services| services.iter().find(|service| service.id.ends_with(endpoint)))
                .map(|service| service.service_endpoint.as_str());
            let key = doc
                .verification_method
                .as_ref()
                .and_then(|methods| methods.iter().find(|method| method.id.ends_with(key)))
                .and_then(|method| method.public_key_multibase.as_deref());
            Some((doc.id, parse_key_endpoint(endpoint, key)?))
        }
        Err(err) => {
            tracing::debug!(?input, %err, "parse error");
            None
        }
    }
}

fn parse_key_endpoint(endpoint: Option<&str>, key: Option<&str>) -> Option<(DidEndpoint, DidKey)> {
    // key can be null for legacy doc formats
    if let Some(key) = key {
        match multibase::decode(key.trim_start_matches("did:key:")) {
            Ok((_, vec)) => match vec.try_into() {
                Ok(key) => {
                    // endpoint can be null for legacy doc formats
                    let pds = endpoint.and_then(|endpoint| {
                        Some(endpoint.strip_prefix("https://")?.trim_end_matches('/').into())
                    });
                    return Some((pds, key));
                }
                Err(_) => {
                    tracing::debug!(%key, "invalid key length");
                }
            },
            Err(err) => {
                tracing::debug!(%key, %err, "invalid key");
            }
        }
    }
    None
}
