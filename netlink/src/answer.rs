//! What tarae waits for after a send, as state the replies are fed into one datagram at a time — the socket's loop
//! is only the reading; what counts as an answer is here, and its tests feed it the kernel's own bytes.
//!
//! A change (`Owed`) is answered when every message that asked for an ack has its ack or its refusal; a refusal of
//! any of its messages (`BATCH_BEGIN`'s too: the kernel refuses a whole batch by its first message) is the outcome —
//! the first one, when rtnetlink refuses more than one.
//! A read (`Reading`) is answered by its dump's end, or by its ack for a single request. A reply about another
//! sequence number — a late answer to an earlier send — is left alone.

use crate::reply::{self, Reply};
use crate::{Batch, Error};

/// A change sent: what it still waits for.
pub(crate) struct Owed<'b> {
    batch: &'b Batch,
    first: u32,
    waiting: Vec<u32>,
    refused: Option<Error>,
}

impl<'b> Owed<'b> {
    /// `batch` sent with its first message numbered `first`.
    pub fn new(batch: &'b Batch, first: u32) -> Owed<'b> {
        let waiting = batch.answered().map(|i| first.wrapping_add(i as u32)).collect();
        Owed { batch, first, waiting, refused: None }
    }

    /// The batch's message numbered `seq`, if it's one of its.
    fn index(&self, seq: u32) -> Option<usize> {
        let i = seq.wrapping_sub(self.first) as usize;
        (i < self.batch.len()).then_some(i)
    }

    /// One datagram's replies.
    pub fn take(&mut self, datagram: &[u8]) -> Result<(), Error> {
        for r in reply::replies(datagram)? {
            match r {
                Reply::Ack { seq } => self.waiting.retain(|s| *s != seq),
                Reply::Refused { seq, errno, offset, said } => {
                    let Some(index) = self.index(seq) else { continue };
                    self.waiting.retain(|s| *s != seq);
                    if self.refused.is_none() {
                        let attribute = offset.and_then(|o| self.batch.attribute_at(index, o));
                        self.refused = Some(Error::Kernel { errno, index: Some(index), attribute, said });
                    }
                }
                Reply::Done { .. } | Reply::Data { .. } => {}
            }
        }
        Ok(())
    }

    /// Every message that asked has its answer.
    pub fn answered(&self) -> bool {
        self.waiting.is_empty()
    }

    /// Some message was refused, and what the kernel says of the batch came with the send. What landed depends on the
    /// protocol: nf_tables takes a batch whole or not at all, so nothing did; rtnetlink takes each message on its own,
    /// so the others landed or were refused each by itself (their answers follow in the same queue).
    pub fn refused(&self) -> bool {
        self.refused.is_some()
    }

    pub fn outcome(self) -> Result<(), Error> {
        self.refused.map_or(Ok(()), Err)
    }
}

/// A message of a read's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Message {
    pub kind: u16,
    pub body: Vec<u8>,
}

/// A read sent (one request, numbered `seq`): what came so far, and whether it ended.
pub(crate) struct Reading {
    seq: u32,
    dump: bool,
    got: Vec<Message>,
    end: Option<Result<(), Error>>,
}

impl Reading {
    /// A request numbered `seq` — a dump (ended by `NLMSG_DONE`) or a single request asking an ack.
    pub fn new(seq: u32, dump: bool) -> Reading {
        Reading { seq, dump, got: Vec::new(), end: None }
    }

    pub fn take(&mut self, datagram: &[u8]) -> Result<(), Error> {
        for r in reply::replies(datagram)? {
            if self.end.is_some() {
                break;
            }
            match r {
                Reply::Data { seq, kind, body } if seq == self.seq => {
                    self.got.push(Message { kind, body: body.to_vec() })
                }
                Reply::Done { seq, errno } if seq == self.seq && self.dump => {
                    self.end = Some(match errno {
                        0 => Ok(()),
                        e => Err(Error::Kernel { errno: e, index: Some(0), attribute: None, said: None }),
                    })
                }
                Reply::Ack { seq } if seq == self.seq && !self.dump => self.end = Some(Ok(())),
                Reply::Refused { seq, errno, said, .. } if seq == self.seq => {
                    self.end = Some(Err(Error::Kernel { errno, index: Some(0), attribute: None, said }))
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn ended(&self) -> bool {
        self.end.is_some()
    }

    pub fn outcome(self) -> Result<Vec<Message>, Error> {
        match self.end {
            Some(Ok(())) => Ok(self.got),
            Some(Err(e)) => Err(e),
            None => Err(Error::Timeout),
        }
    }
}
