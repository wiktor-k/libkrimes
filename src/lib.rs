mod asn1;
pub mod proto;

use bytes::BufMut;
use bytes::BytesMut;
use std::io::{self};
use tokio_util::codec::{Decoder, Encoder};
use xdr_codec::record::XdrRecordWriter;
use xdr_codec::Write;

use crate::proto::KerberosRequest;

const DEFAULT_MAX_SIZE: usize = 32 * 1024;

pub struct KerberosTcpCodec {
    max_size: usize,
}

impl Default for KerberosTcpCodec {
    fn default() -> Self {
        KerberosTcpCodec {
            max_size: DEFAULT_MAX_SIZE
        }
    }
}

impl Decoder for KerberosTcpCodec {
    type Item = KerberosRequest;
    type Error = io::Error;

    fn decode(&mut self, _buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // How many bytes to consume?
        todo!();
    }
}

impl Encoder<KerberosRequest> for KerberosTcpCodec {
    type Error = io::Error;

    fn encode(&mut self, msg: KerberosRequest, buf: &mut BytesMut) -> io::Result<()> {
        debug_assert!(buf.is_empty());

        let der_bytes = msg
            .to_der()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        debug_assert!(buf.len() <= self.max_size);

        /* RFC1831 section 10
        *
        * When RPC messages are passed on top of a byte stream transport
        * protocol (like TCP), it is necessary to delimit one message from
        * another in order to detect and possibly recover from protocol errors.
        * This is called record marking (RM).  One RPC message fits into one RM
        * record.

        * A record is composed of one or more record fragments.  A record
        * fragment is a four-byte header followed by 0 to (2**31) - 1 bytes of
        * fragment data.  The bytes encode an unsigned binary number; as with
        * XDR integers, the byte order is from highest to lowest.  The number
        * encodes two values -- a boolean which indicates whether the fragment
        * is the last fragment of the record (bit value 1 implies the fragment
        * is the last fragment) and a 31-bit unsigned binary value which is the
        * length in bytes of the fragment's data.  The boolean value is the
        * highest-order bit of the header; the length is the 31 low-order bits.
        * (Note that this record specification is NOT in XDR standard form!)
        */
        let mut w = XdrRecordWriter::new(buf.writer());
        w.write_all(&der_bytes)?;

        // XdrRecordWriter flushess on drop with end of record bit set (bit 32 in header)
        // MIT KDC interpret this bit as part of record length and produces the
        // following error:
        //  krb5kdc[1](Error): TCP client 172.17.0.1.41668 wants 2147483817 bytes, cap is 1048572
        // Explicitly flush without EOR bit
        w.flush_eor(false)
    }
}

#[cfg(test)]
mod tests {
    use futures::SinkExt;
    use tokio::net::TcpStream;
    use tokio_util::codec::Framed;

    use std::time::{Duration, SystemTime};

    use super::KerberosTcpCodec;
    use crate::proto::KerberosRequest;

    #[tokio::test]
    async fn test_localhost_kdc() {
        let _ = tracing_subscriber::fmt::try_init();

        let stream = TcpStream::connect("127.0.0.1:55000")
            .await
            .expect("Unable to connect to localhost:55000");

        let mut krb_stream = Framed::new(stream, KerberosTcpCodec::default());

        let as_req = KerberosRequest::build_asreq(
            "testuser".to_string(),
            "krbtgt".to_string(),
            None,
            SystemTime::now() + Duration::from_secs(3600),
            None,
        )
        .build();

        // Write a request
        krb_stream
            .send(as_req)
            .await
            .expect("Failed to transmit request");

        // What did we get back?
        //let response = krb_stream.next().await;

        //trace!(?response);
    }
}
