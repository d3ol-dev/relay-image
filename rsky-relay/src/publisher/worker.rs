use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::{io, thread};

use bytes::Bytes;
use fjall::{PartitionCreateOptions, PartitionHandle};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use thiserror::Error;

use crate::SHUTDOWN;
use crate::publisher::connection::{Connection, ConnectionError};
use crate::publisher::types::{Command, CommandReceiver};
use crate::types::{Cursor, DB};

const INTEREST: Interest = Interest::WRITABLE;

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("connection error: {0}")]
    ConnectionError(#[from] ConnectionError),
    #[error("fjall error: {0}")]
    Fjall(#[from] fjall::Error),
}

pub struct Worker {
    id: usize,
    connections: Vec<Option<Connection>>,
    next_idx: usize,
    command_rx: CommandReceiver,
    firehose: PartitionHandle,
    poll: Poll,
    events: Events,
}

impl Worker {
    pub fn new(id: usize, command_rx: CommandReceiver) -> Result<Self, WorkerError> {
        let firehose = DB.open_partition("firehose", PartitionCreateOptions::default())?;
        let poll = Poll::new()?;
        let events = Events::with_capacity(1024);
        Ok(Self { id, connections: Vec::new(), next_idx: 0, command_rx, firehose, poll, events })
    }

    pub fn run(mut self) -> Result<(), WorkerError> {
        let span = tracing::info_span!("publisher", id = %self.id);
        let _enter = span.enter();
        let mut seq = self.firehose.last_key_value()?.map(|(k, _)| k.into()).unwrap_or_default();
        while self.update(&mut seq)? {
            thread::yield_now();
        }
        tracing::info!("shutting down");
        self.shutdown();
        Ok(())
    }

    pub fn shutdown(self) {
        for conn in self.connections {
            drop(conn);
        }
    }

    fn handle_command(&mut self, command: Command, mut seq: Cursor) {
        match command {
            Command::Connect(config) => {
                tracing::info!(addr = %config.addr, cursor = ?config.cursor, "starting publish");
                match Connection::connect(
                    config.addr,
                    config.stream,
                    config.cursor.unwrap_or_else(|| seq.next()),
                ) {
                    Ok(conn) => {
                        let idx = self.connections.iter().position(Option::is_none).unwrap_or_else(
                            || {
                                let idx = self.connections.len();
                                self.connections.push(None);
                                idx
                            },
                        );
                        #[expect(clippy::expect_used)]
                        self.poll
                            .registry()
                            .register(&mut SourceFd(&conn.as_raw_fd()), Token(idx), INTEREST)
                            .expect("unable to register");
                        self.connections[idx] = Some(conn);
                    }
                    Err(err) => {
                        tracing::warn!(addr = %config.addr, cursor = ?config.cursor, %err, "unable to subscribeRepos");
                    }
                }
            }
        }
    }

    fn update(&mut self, seq: &mut Cursor) -> Result<bool, WorkerError> {
        if SHUTDOWN.load(Ordering::Relaxed) {
            return Ok(false);
        }

        for _ in 0..32 {
            if let Ok(command) = self.command_rx.pop() {
                self.handle_command(command, *seq);
            }

            for msg in self.firehose.range((*seq + 1)..=(*seq + 32)) {
                let (k, v) = msg?;
                *seq = k.into();
                self.send(*seq, &Bytes::from_owner(v));
            }

            let mut events = std::mem::replace(&mut self.events, Events::with_capacity(0));
            'outer: for _ in 0..32 {
                #[expect(clippy::expect_used)]
                self.poll
                    .poll(&mut events, Some(Duration::from_millis(1)))
                    .expect("failed to poll");
                for ev in &events {
                    if !self.poll(*seq, ev.token().0) {
                        break 'outer;
                    }
                }
            }
            self.events = events;
        }

        for _ in 0..self.connections.len() {
            self.next_idx = (self.next_idx + 1) % self.connections.len();
            if !self.poll(*seq, self.next_idx) {
                break;
            }
        }

        Ok(true)
    }

    fn send(&mut self, seq: Cursor, data: &Bytes) -> bool {
        for conn in &mut self.connections {
            if let Some(inner) = conn.as_mut() {
                if let Err(err) = inner.send(seq, data.clone()) {
                    tracing::info!(addr = %inner.addr, cursor = %inner.cursor, %err, "disconnected");
                    #[expect(clippy::expect_used)]
                    self.poll
                        .registry()
                        .deregister(&mut SourceFd(&inner.as_raw_fd()))
                        .expect("failed to deregister");
                    *conn = None;
                }
            }
        }
        true
    }

    fn poll(&mut self, seq: Cursor, idx: usize) -> bool {
        if let Some(conn) = &mut self.connections[idx] {
            match conn.poll(seq, &self.firehose) {
                Ok(true) => return true,
                Ok(false) => {
                    tracing::info!(addr = %conn.addr, cursor = %conn.cursor, "closed due to invalid cursor");
                }
                Err(err) => {
                    tracing::info!(addr = %conn.addr, cursor = %conn.cursor, %err, "disconnected");
                }
            }
            #[expect(clippy::expect_used)]
            self.poll
                .registry()
                .deregister(&mut SourceFd(&conn.as_raw_fd()))
                .expect("failed to deregister");
            self.connections[idx] = None;
        }

        true
    }
}
