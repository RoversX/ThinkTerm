use crate::budget::RouteBudget;
use crate::channel::{parse, Channel, Deadline};
use crate::protocol::{Command, Reply};
use crate::wire::{Assembly, Frame, Kind, Payload, MAX_BLOCK};
use crate::{Error, Result, MAX_READ_BYTES};
use futures::io::{AsyncRead, AsyncWrite};

/// The outer `Result` of these functions is the channel's: after an error it
/// is out of step with its peer and has to close. This inner one is the
/// request's: the request's frames were consumed, so the channel carries on
/// and only that request is refused.
pub type Refusable<T> = std::result::Result<T, Error>;

pub async fn receive_input<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut Channel<S>,
    command: &Command,
    sequence: u64,
    budget: &RouteBudget,
    deadline: &Deadline,
) -> Result<Refusable<Option<Payload>>> {
    command.validate()?;
    match command {
        Command::Text { bytes, .. } => Ok(receive_payload(
            channel,
            *bytes,
            sequence,
            Kind::InputBlock,
            Assembly::input(*bytes, budget),
            deadline,
        )
        .await?
        .map(Some)),
        _ => Ok(Ok(None)),
    }
}

pub async fn receive_reply<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut Channel<S>,
    sequence: u64,
    reserve: impl FnOnce(usize) -> Result<Assembly>,
    deadline: &Deadline,
) -> Result<Refusable<(Reply, Option<Payload>)>> {
    let reply: Reply = parse(&channel.read(Some(deadline)).await?, Kind::Reply, sequence)?;
    let payload = match &reply {
        Reply::Text { bytes } => {
            if *bytes > MAX_READ_BYTES {
                return Err(Error::Protocol);
            }
            match receive_payload(
                channel,
                *bytes,
                sequence,
                Kind::TextBlock,
                reserve(*bytes),
                deadline,
            )
            .await?
            {
                Ok(payload) => Some(payload),
                Err(error) => return Ok(Err(error)),
            }
        }
        Reply::List { exports, .. } => {
            if exports.len() > 32
                || exports.iter().any(|item| {
                    item.name.len() > 128
                        || item.title.len() > 1024
                        || item.remaining_ms > crate::MAX_DURATION_SECS * 1000
                        || crate::protocol::display_text(&item.name, 128) != item.name
                        || crate::protocol::display_text(&item.title, 1024) != item.title
                })
            {
                return Err(Error::Protocol);
            }
            None
        }
        _ => None,
    };
    Ok(Ok((reply, payload)))
}

/// Consume the rest of a reply whose text is not wanted, to stay in step.
pub async fn discard_text<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut Channel<S>,
    mut received: usize,
    total: usize,
    sequence: u64,
    deadline: &Deadline,
) -> Result<()> {
    while received < total {
        let frame = channel.read(Some(deadline)).await?;
        match next_block(&frame, Kind::TextBlock, sequence, received, total)? {
            Some(bytes) => received += bytes.len(),
            None => break,
        }
    }
    Ok(())
}

/// The bytes of the next block of a payload, checked against what came
/// before them. None is a sender giving up on its text between two blocks.
pub fn next_block<'a>(
    frame: &'a Frame,
    kind: Kind,
    sequence: u64,
    received: usize,
    total: usize,
) -> Result<Option<&'a [u8]>> {
    if kind == Kind::TextBlock
        && frame.kind() == Kind::Cancel
        && frame.sequence() == sequence
        && frame.body().is_empty()
    {
        return Ok(None);
    }
    if frame.kind() != kind || frame.sequence() != sequence || frame.body().len() <= 4 {
        return Err(Error::Protocol);
    }
    let offset =
        u32::from_be_bytes(frame.body()[..4].try_into().map_err(|_| Error::Protocol)?) as usize;
    let bytes = &frame.body()[4..];
    if offset != received || bytes.len() > total - received {
        return Err(Error::Protocol);
    }
    Ok(Some(bytes))
}

/// A refused `assembly` still reads every block, into nothing.
async fn receive_payload<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut Channel<S>,
    total: usize,
    sequence: u64,
    kind: Kind,
    mut assembly: Refusable<Assembly>,
    deadline: &Deadline,
) -> Result<Refusable<Payload>> {
    let mut received = 0;
    while received < total {
        let frame = channel.read(Some(deadline)).await?;
        let Some(bytes) = next_block(&frame, kind, sequence, received, total)? else {
            return Ok(Err(Error::Unavailable));
        };
        if let Ok(assembly) = &mut assembly {
            assembly.append(received, bytes)?;
        }
        received += bytes.len();
    }
    match assembly {
        Ok(assembly) => Ok(Ok(assembly.finish()?)),
        Err(error) => Ok(Err(error)),
    }
}

pub async fn send_payload<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut Channel<S>,
    bytes: &[u8],
    sequence: u64,
    kind: Kind,
    deadline: &Deadline,
) -> Result<()> {
    let limit = if kind == Kind::InputBlock {
        crate::MAX_INPUT_BYTES
    } else if kind == Kind::TextBlock {
        crate::MAX_READ_BYTES
    } else {
        return Err(Error::Protocol);
    };
    if bytes.len() > limit {
        return Err(Error::TooLarge);
    }
    for (index, block) in bytes.chunks(MAX_BLOCK).enumerate() {
        let mut body = Vec::with_capacity(block.len() + 4);
        body.extend_from_slice(&((index * MAX_BLOCK) as u32).to_be_bytes());
        body.extend_from_slice(block);
        channel
            .write(&Frame::new(kind, sequence, &body)?, deadline)
            .await?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::budget::Budgets;
    use crate::channel::Deadline;
    use crate::Id;
    use std::os::unix::net::UnixStream;

    type Pair = (
        Channel<smol::Async<UnixStream>>,
        Channel<smol::Async<UnixStream>>,
    );

    fn pair(budget: &RouteBudget) -> Pair {
        let (near, far) = smol::Async::<UnixStream>::pair().unwrap();
        (
            Channel::new(near, budget.acquire(1024 * 1024).unwrap()).unwrap(),
            Channel::new(far, budget.acquire(1024 * 1024).unwrap()).unwrap(),
        )
    }

    fn text_reply(bytes: usize) -> Reply {
        Reply::Text { bytes }
    }

    #[test]
    fn a_refused_payload_is_consumed_so_the_next_frame_is_read_in_step() {
        let budgets = Budgets::new().unwrap();
        let budget = budgets.open_route(Id::random().unwrap()).unwrap();
        let (mut near, mut far) = pair(&budget);
        let deadline = Deadline::request().unwrap();
        let text = "example".repeat(10_000);
        smol::block_on(async {
            let send = async {
                near.write_json(Kind::Reply, 1, &text_reply(text.len()), &deadline)
                    .await?;
                send_payload(&mut near, text.as_bytes(), 1, Kind::TextBlock, &deadline).await?;
                near.write_json(Kind::Reply, 2, &text_reply(0), &deadline)
                    .await
            };
            let receive = async {
                let refused = receive_reply(&mut far, 1, |_| Err(Error::Busy), &deadline).await?;
                let next = receive_reply(
                    &mut far,
                    2,
                    |bytes| Assembly::text(bytes, &budget),
                    &deadline,
                )
                .await?;
                Ok::<_, Error>((refused, next))
            };
            let (sent, received) = smol::future::zip(send, receive).await;
            sent.unwrap();
            let (refused, next) = received.unwrap();
            assert_eq!(refused.err(), Some(Error::Busy));
            assert!(next.is_ok());
        });
    }

    #[test]
    fn a_sender_can_give_up_between_blocks_without_losing_the_channel() {
        let budgets = Budgets::new().unwrap();
        let budget = budgets.open_route(Id::random().unwrap()).unwrap();
        let (mut near, mut far) = pair(&budget);
        let deadline = Deadline::request().unwrap();
        smol::block_on(async {
            near.write_json(Kind::Reply, 1, &text_reply(100_000), &deadline)
                .await
                .unwrap();
            send_payload(&mut near, &[b'x'; 100], 1, Kind::TextBlock, &deadline)
                .await
                .unwrap();
            near.write(&Frame::new(Kind::Cancel, 1, &[]).unwrap(), &deadline)
                .await
                .unwrap();
            let received = receive_reply(
                &mut far,
                1,
                |bytes| Assembly::text(bytes, &budget),
                &deadline,
            )
            .await
            .unwrap();
            assert_eq!(received.err(), Some(Error::Unavailable));
        });
        // The reservation of the abandoned text went with it.
        assert_eq!(budgets.retained().unwrap().0, 2);
    }

    #[test]
    fn a_reply_joins_its_requests_reservation_without_a_new_slot_or_rate_charge() {
        let budgets = Budgets::new().unwrap();
        let budget = budgets.open_route(Id::random().unwrap()).unwrap();
        for _ in 0..19 {
            drop(budget.acquire(0).unwrap());
        }
        let request = budget.acquire(64).unwrap();
        // The rate limit is spent; the request it paid for still gets through.
        assert!(matches!(budget.acquire(0), Err(Error::Busy)));
        let assembly = Assembly::reply_text(1024, request).unwrap();
        assert_eq!(budgets.retained().unwrap(), (1, 1088));
        drop(assembly);
        assert_eq!(budgets.retained().unwrap(), (0, 0));
    }

    #[test]
    fn a_block_out_of_place_is_a_protocol_error_even_when_discarding() {
        let budgets = Budgets::new().unwrap();
        let budget = budgets.open_route(Id::random().unwrap()).unwrap();
        let (mut near, mut far) = pair(&budget);
        let deadline = Deadline::request().unwrap();
        smol::block_on(async {
            let mut body = 7u32.to_be_bytes().to_vec();
            body.extend_from_slice(b"example");
            near.write(&Frame::new(Kind::TextBlock, 1, &body).unwrap(), &deadline)
                .await
                .unwrap();
            assert_eq!(
                discard_text(&mut far, 0, 100, 1, &deadline).await,
                Err(Error::Protocol)
            );
        });
    }
}
