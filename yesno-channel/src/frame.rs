//! Reading frames off a stream.
//!
//! Lifted out of `yesno-plugin`'s `channel` module with the client, because the
//! client needs it and the host crate is the half a peer must not link. The
//! host re-exports it, so `yesno_plugin::channel::read_frame` still resolves.
use crate::ipc::Frame;

/// Read one frame from `r`, growing `buf` until a whole frame is present.
///
/// `Ok( None )` on a clean end of stream, which is the peer having gone away -- the
/// signal that the session may be dropped and its snapshots released.
pub fn read_frame<R: std::io::Read>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<Frame>> {
    loop {
        match Frame::decode(buf) {
            Ok((f, used)) => {
                buf.drain(..used);
                return Ok(Some(f));
            }
            Err(crate::ipc::IpcError::Truncated) => {}
            Err(e) => {
                // The host crate has a trait for this; duplicating one line
                // is cheaper than exporting it just so a peer can decode.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                ));
            }
        }
        let mut chunk = [0u8; 4096];
        let n = r.read(&mut chunk)?;
        if n == 0 {
            // A clean close with nothing buffered is the peer leaving. A clean close
            // mid-frame is a peer that died between writes, which is the same
            // outcome for us and not worth a different error.
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}
