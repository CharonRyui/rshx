//! Reading a Host's two streams.
//!
//! Both are read at once — read in turn, they deadlock once the child fills one
//! — and both are capped: a Host that prints more than rshx keeps must not grow
//! rshx without bound, nor block on a full pipe, since ssh would then never
//! exit.
use tokio::io::AsyncReadExt;

/// The most of each stream kept in memory; past this the bytes are dropped.
pub(super) const CAP: usize = 1024 * 1024;

/// Reads a stream, keeping at most `cap` bytes. The rest is drained and
/// thrown away: a Host that prints more than the cap must not grow rshx
/// without bound, nor block on a full pipe, since ssh would then never exit.
pub(crate) async fn read_capped<R>(mut stream: R, cap: usize) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                let keep = room.min(n);
                kept.extend_from_slice(&buf[..keep]);
                truncated |= keep < n;
            }
            // The status comes from ssh's exit status, never a read error.
            Err(_) => break,
        }
    }
    (kept, truncated)
}
