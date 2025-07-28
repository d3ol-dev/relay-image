use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant, SystemTimeError};

use chrono::{DateTime, Utc};
use fjall::{Batch, PartitionCreateOptions, PartitionHandle, PersistMode};
use hashbrown::HashMap;
#[cfg(not(feature = "labeler"))]
use hashbrown::hash_map::Entry;
use rusqlite::Connection;
use thiserror::Error;

use crate::SHUTDOWN;
use crate::config::HOSTS_WRITE_INTERVAL;
use crate::types::{Cursor, DB, MessageReceiver};
use crate::validator::event::{ParseError, SerializeError, SubscribeReposEvent};
use crate::validator::resolver::{Resolver, ResolverError};
#[cfg(not(feature = "labeler"))]
use crate::validator::types::RepoState;
use crate::validator::utils;

const SLEEP: Duration = Duration::from_micros(100);

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("serialize error: {0}")]
    Serialize(#[from] SerializeError),
    #[error("resolver error: {0}")]
    Resolver(#[from] ResolverError),
    #[error("time error: {0}")]
    Time(#[from] SystemTimeError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("fjall error: {0}")]
    Fjall(#[from] fjall::Error),
    #[error("decode error: {0}")]
    DecodeError(#[from] serde_ipld_dagcbor::DecodeError<Infallible>),
}

pub struct Manager {
    message_rx: MessageReceiver,
    hosts: HashMap<String, (Cursor, DateTime<Utc>)>,
    #[cfg(not(feature = "labeler"))]
    repos: HashMap<String, RepoState>,
    resolver: Resolver,
    last: Instant,
    conn: Connection,
    queue: PartitionHandle,
    firehose: PartitionHandle,
}

impl Manager {
    pub fn new(message_rx: MessageReceiver) -> Result<Self, ManagerError> {
        let hosts = HashMap::new();
        #[cfg(not(feature = "labeler"))]
        let repos = HashMap::new();
        let resolver = Resolver::new()?;
        let now = Instant::now();
        let last = now.checked_sub(HOSTS_WRITE_INTERVAL).unwrap_or(now);
        let conn = Connection::open("relay.db")?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS hosts (
                host TEXT PRIMARY KEY,
                cursor INTEGER NOT NULL,
                latest TEXT NOT NULL
            )",
            (),
        )?;
        let queue = DB.open_partition("queue", PartitionCreateOptions::default())?;
        let firehose = DB.open_partition("firehose", PartitionCreateOptions::default())?;
        Ok(Self {
            message_rx,
            hosts,
            #[cfg(not(feature = "labeler"))]
            repos,
            resolver,
            last,
            conn,
            queue,
            firehose,
        })
    }

    pub async fn run(mut self) -> Result<(), ManagerError> {
        let mut hosts = 0;
        {
            let mut stmt = self.conn.prepare_cached("SELECT host, cursor FROM hosts")?;
            let mut rows = stmt.query(())?;
            while let Some(row) = rows.next()? {
                let host = row.get_unwrap("host");
                let cursor: u64 = row.get_unwrap("cursor");
                self.hosts.insert(host, (cursor.into(), DateTime::UNIX_EPOCH));
                hosts += 1;
            }
        }
        #[allow(unused_mut)]
        let mut repos = 0;
        #[cfg(not(feature = "labeler"))]
        {
            // TODO: move this to sqlite
            let handle = DB.open_partition("repos", PartitionCreateOptions::default())?;
            self.repos.reserve(handle.approximate_len());
            for res in handle.iter() {
                let (did, state) = res?;
                #[expect(clippy::unwrap_used)]
                let did = String::from_utf8(did.to_vec()).unwrap();
                let state = serde_ipld_dagcbor::from_slice(&state)?;
                self.repos.insert(did, state);
                repos += 1;
            }
        }

        let mut cursor = self.firehose.last_key_value()?.map(|(k, _)| k.into()).unwrap_or_default();
        let mut queue_drained = 0;
        let mut queue_pending = 0;
        for res in self.queue.keys() {
            let key = res?;
            #[expect(clippy::unwrap_used)]
            let key = std::str::from_utf8(&key).unwrap();
            #[expect(clippy::unwrap_used)]
            let did = key.split('>').next().unwrap();
            if self.resolver.resolve(did)?.is_some() {
                self.scan_did(&mut cursor, did)?;
                queue_drained += 1;
            } else {
                queue_pending += 1;
            }
        }

        tracing::info!(%hosts, %repos, %queue_drained, %queue_pending, %cursor, "loaded state");
        while self.update(&mut cursor).await? {}
        tracing::info!("shutting down validator");
        SHUTDOWN.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn persist(&mut self) -> Result<(), ManagerError> {
        // persist hosts data
        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare_cached(
            "
                INSERT INTO hosts (host, cursor, latest)
                VALUES (?1, ?2, ?3)
                ON CONFLICT(host)
                DO UPDATE SET cursor = excluded.cursor, latest = excluded.latest
            ",
        )?;
        for (host, (cursor, time)) in &self.hosts {
            if *time != DateTime::UNIX_EPOCH {
                stmt.execute((host, cursor.get(), time))?;
            }
        }
        drop(stmt);
        tx.commit()?;

        Ok(())
    }

    #[expect(clippy::too_many_lines)]
    async fn update(&mut self, cursor: &mut Cursor) -> Result<bool, ManagerError> {
        if SHUTDOWN.load(Ordering::Relaxed) {
            return Ok(false);
        }

        let now = Instant::now();
        if self.last + HOSTS_WRITE_INTERVAL < now {
            self.persist()?;
            self.last = now;
        }

        for _ in 0..1024 {
            let msg = match self.message_rx.try_recv_ref() {
                Ok(msg) => msg,
                Err(thingbuf::mpsc::errors::TryRecvError::Empty) => {
                    thread::sleep(SLEEP);
                    break;
                }
                Err(thingbuf::mpsc::errors::TryRecvError::Closed) => return Ok(false),
                Err(_) => unreachable!(),
            };

            let host = &msg.hostname;
            let span = tracing::info_span!("msg_recv", %host, len = %msg.data.len());
            let _enter = span.enter();
            let event = match SubscribeReposEvent::parse(&msg.data) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(err) => {
                    tracing::debug!(%err, "parse error");
                    continue;
                }
            };

            // check/record per-host seq/time
            let type_ = event.type_();
            let seq = event.seq();
            let mut time = event.time();
            let did = event.did();
            let span = tracing::debug_span!("msg_data", type = %type_, %seq, %time, %did);
            let _enter = span.enter();
            if let Some((prev, old)) = self.hosts.get(host) {
                time = time.max(*old);
                let prev: u64 = (*prev).into();
                let curr: u64 = seq.into();
                if prev >= curr {
                    if prev > curr {
                        tracing::trace!(%prev, diff = %prev - curr, "old seq");
                    }
                    continue;
                } else if prev + 1 != curr {
                    tracing::trace!(%prev, diff = %curr - prev - 1, "seq gap");
                }
            }

            // get commit object for #commit/#sync or add to the firehose
            let span;
            let _enter;
            #[allow(unused_variables)]
            let (commit, head) = match event.commit() {
                Ok(Some((commit, head))) => {
                    #[cfg(not(feature = "labeler"))]
                    {
                        span = tracing::debug_span!("validate", rev = %commit.rev, data = %commit.data, %head);
                    }
                    #[cfg(feature = "labeler")]
                    {
                        span = tracing::debug_span!("validate", n_labels = commit.len());
                    }
                    _enter = span.enter();

                    #[cfg(not(feature = "labeler"))]
                    if !event.validate(&commit, &head) {
                        continue;
                    }
                    (commit, head)
                }
                Ok(None) => {
                    if let SubscribeReposEvent::Identity(_) = &event {
                        self.resolver.expire(did, event.time());
                    }
                    let data = event.serialize(msg.data.len(), cursor.next())?;
                    self.firehose.insert(*cursor, data)?;
                    self.hosts.insert(host.clone(), (seq, time));
                    continue;
                }
                Err(err) => {
                    tracing::debug!(%err, "commit decode error");
                    continue;
                }
            };

            // resolve identity & check pds
            let Some((pds, key)) = self.resolver.resolve(did)? else {
                self.queue.insert(format!("{did}>{host}>{seq}"), msg.data.to_vec())?;
                self.hosts.insert(host.clone(), (seq, time));
                continue;
            };

            if let Some(pds) = pds {
                if host != pds {
                    // expire the identity & queue message in case the user has migrated
                    self.resolver.expire(did, time);
                    self.queue.insert(format!("{did}>{host}>{seq}"), msg.data.to_vec())?;
                    self.hosts.insert(host.clone(), (seq, time));
                    continue;
                }
            }

            // verify signature
            #[allow(clippy::needless_borrow)]
            match utils::verify_commit_sig(&commit, key) {
                Ok(valid) => {
                    if !valid {
                        tracing::debug!(?key, "signature mismatch");
                        continue;
                    }
                }
                Err(err) => {
                    tracing::debug!(%err, ?key, "signature check error");
                    continue;
                }
            }

            // verify commit message
            #[cfg(not(feature = "labeler"))]
            let (rev, data, entry) = { (commit.rev, commit.data, self.repos.entry(commit.did)) };
            #[cfg(not(feature = "labeler"))]
            if let SubscribeReposEvent::Commit(commit) = &event {
                // TODO: should still validate records existing in blocks, etc
                if let Entry::Occupied(prev) = &entry {
                    let prev = prev.get();
                    let span = tracing::debug_span!("previous", rev = %prev.rev, data = %prev.data, head = %prev.head);
                    let _enter = span.enter();
                    if !utils::verify_commit_event(commit, data, prev) {
                        continue;
                    }
                }
            }

            let msg = event.serialize(msg.data.len(), cursor.next())?;
            self.firehose.insert(*cursor, msg)?;
            #[cfg(not(feature = "labeler"))]
            entry.insert(RepoState { rev, data, head });
            self.hosts.insert(host.clone(), (seq, time));
        }

        for did in self.resolver.poll().await? {
            self.scan_did(cursor, &did)?;
        }

        Ok(true)
    }

    fn scan_did(&mut self, cursor: &mut Cursor, did: &str) -> Result<(), ManagerError> {
        let Some((pds, key)) = self.resolver.resolve(did)? else { unreachable!("{did}") };

        let mut batch: Option<Batch> = None;
        for res in self.queue.prefix(&did) {
            let (k, input) = res?;
            batch.get_or_insert_with(|| DB.batch()).remove(&self.queue, k.clone());

            #[expect(clippy::unwrap_used)]
            let host = std::str::from_utf8(&k).unwrap().split('>').nth(1).unwrap();
            let span = tracing::debug_span!("msg_read", %host, len = %input.len());
            let _enter = span.enter();

            #[expect(clippy::unwrap_used)]
            let event = SubscribeReposEvent::parse(&input)?.unwrap(); // already parsed
            let type_ = event.type_();
            let seq = event.seq();
            let time = event.time();
            let did = event.did();
            let span = tracing::debug_span!("msg_data", type = %type_, %seq, %time, %did);
            let _enter = span.enter();

            #[allow(unused_variables)]
            #[expect(clippy::unwrap_used)]
            let (commit, head) = event.commit()?.unwrap(); // already parsed
            #[cfg(not(feature = "labeler"))]
            let span =
                tracing::debug_span!("validate", rev = %commit.rev, data = %commit.data, %head);
            #[cfg(feature = "labeler")]
            let span = tracing::debug_span!("validate", n_labels = commit.len());
            let _enter = span.enter();

            if let Some(pds) = pds {
                if host != pds {
                    tracing::debug!(%pds, "hostname pds mismatch");
                    continue;
                }
            }

            // verify signature
            #[allow(clippy::needless_borrow)]
            match utils::verify_commit_sig(&commit, key) {
                Ok(valid) => {
                    if !valid {
                        tracing::debug!(?key, "signature mismatch");
                        continue;
                    }
                }
                Err(err) => {
                    tracing::debug!(%err, ?key, "signature check error");
                    continue;
                }
            }

            // verify commit message
            #[cfg(not(feature = "labeler"))]
            let (rev, data, entry) = { (commit.rev, commit.data, self.repos.entry(commit.did)) };
            #[cfg(not(feature = "labeler"))]
            if let SubscribeReposEvent::Commit(commit) = &event {
                // TODO: should still validate records existing in blocks, etc
                if let Entry::Occupied(prev) = &entry {
                    let prev = prev.get();
                    let span = tracing::debug_span!("previous", rev = %prev.rev, data = %prev.data, head = %prev.head);
                    let _enter = span.enter();
                    if !utils::verify_commit_event(commit, data, prev) {
                        continue;
                    }
                }
            }

            let msg = event.serialize(input.len(), cursor.next())?;
            self.firehose.insert(*cursor, msg)?;
            #[cfg(not(feature = "labeler"))]
            entry.insert(RepoState { rev, data, head });
        }
        if let Some(batch) = batch {
            batch.commit()?;
        }

        Ok(())
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        SHUTDOWN.store(true, Ordering::Relaxed);

        if let Err(err) = self.persist() {
            tracing::warn!(%err, "unable to persist host state\n{:#?}", self.hosts);
        }

        #[cfg(not(feature = "labeler"))]
        match DB.open_partition("repos", PartitionCreateOptions::default()) {
            Ok(repos) => {
                let len = self.repos.len();
                let mut batch = Batch::with_capacity(DB.clone(), len);
                for (did, state) in self.repos.drain() {
                    #[expect(clippy::unwrap_used)]
                    batch.insert(
                        &repos,
                        did.into_bytes(),
                        serde_ipld_dagcbor::to_vec(&state).unwrap(),
                    );
                }
                tracing::info!(%len, "persisting repos");
                if let Err(err) = batch.commit() {
                    tracing::warn!(%err, "unable to persist repo state");
                }
            }
            Err(err) => {
                tracing::warn!(%err, "unable to open repos tree");
            }
        }

        if let Err(err) = DB.persist(PersistMode::SyncAll) {
            tracing::warn!(%err, "unable to flush db");
        }
    }
}
