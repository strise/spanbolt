//! The Spanner gRPC service, with ExecuteStreamingSql and StreamingRead sending pre-encoded
//! partials.
//!
//! The generated `SpannerServer` encodes every response with prost, so a streaming RPC would
//! have to hold each PartialResultSet as a message tree until gRPC encodes it. [`SpannerRouter`]
//! serves those two methods itself with [`EncodedCodec`], which writes the bytes the
//! [`PartialWriter`](crate::stream::PartialWriter) produced as they are, and passes every other
//! method on to the generated server.
//!
//! Their response bodies go out in DATA frames of at most [`MAX_DATA_FRAME`] bytes, like the
//! emulator's Go server writes. h2 would otherwise send each 1 MiB partial as one frame up to the
//! client's SETTINGS_MAX_FRAME_SIZE, and grpc-core fails the RPC ("frame of size … overflows
//! local window") when it shrinks its stream window while such a frame is in flight.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use prost::Message;
use prost::bytes::{BufMut, Bytes};
use spanbolt_proto::spanner as pb;
use spanbolt_proto::spanner::spanner_server::SpannerServer;
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::{BoxFuture, Service, StdError, http};
use tonic::server::{Grpc, NamedService, ServerStreamingService};

use crate::services::spanner::SpannerService;
use crate::stream::ResultStream;

const EXECUTE_STREAMING_SQL: &str = "/google.spanner.v1.Spanner/ExecuteStreamingSql";
const STREAMING_READ: &str = "/google.spanner.v1.Spanner/StreamingRead";

/// The Spanner service: the two streaming methods here, the rest through `SpannerServer`.
#[derive(Clone)]
pub struct SpannerRouter {
    svc: Arc<SpannerService>,
    generated: SpannerServer<SpannerService>,
    max_message: usize,
}

impl SpannerRouter {
    pub fn new(svc: SpannerService, max_message: usize) -> Self {
        let svc = Arc::new(svc);
        SpannerRouter {
            generated: SpannerServer::from_arc(svc.clone())
                .max_decoding_message_size(max_message)
                .max_encoding_message_size(max_message),
            svc,
            max_message,
        }
    }

    fn grpc<D: Message + Default + Send + 'static>(&self) -> Grpc<EncodedCodec<D>> {
        Grpc::new(EncodedCodec(PhantomData))
            .apply_max_message_size_config(Some(self.max_message), Some(self.max_message))
    }
}

impl NamedService for SpannerRouter {
    const NAME: &'static str = <SpannerServer<SpannerService> as NamedService>::NAME;
}

impl<B> Service<http::Request<B>> for SpannerRouter
where
    B: tonic::codegen::Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let compat = self.svc.emu.emulator_compat;
        match req.uri().path() {
            EXECUTE_STREAMING_SQL => {
                let mut grpc = self.grpc::<pb::ExecuteSqlRequest>();
                let method = Method(self.svc.clone(), Sql);
                Box::pin(async move {
                    let res = grpc.server_streaming(method, req).await;
                    Ok(small_frames(undecodable_request(res, compat)))
                })
            }
            STREAMING_READ => {
                let mut grpc = self.grpc::<pb::ReadRequest>();
                let method = Method(self.svc.clone(), Read);
                Box::pin(async move {
                    let res = grpc.server_streaming(method, req).await;
                    Ok(small_frames(undecodable_request(res, compat)))
                })
            }
            _ => {
                let call = self.generated.call(req);
                Box::pin(async move { Ok(undecodable_request(call.await?, compat)) })
            }
        }
    }
}

/// The start of the status tonic answers a request prost cannot decode with (INTERNAL).
const DECODE_ERROR: &str = "failed to decode Protobuf message";

/// Answers a request that does not decode the way Spanner does: INVALID_ARGUMENT with its own
/// message (prost and Spanner's C++ protobuf both stop at 100 nested messages, a STRUCT parameter
/// type 33 levels deep). The emulator answers INTERNAL without a message. tonic answers with the
/// status in the response headers, before any handler runs.
fn undecodable_request(
    mut res: http::Response<tonic::body::Body>,
    compat: bool,
) -> http::Response<tonic::body::Body> {
    if res.headers().get("grpc-status").map(|v| v.as_bytes()) != Some(b"13") {
        return res;
    }
    let Some(status) = Status::from_header_map(res.headers()) else {
        return res;
    };
    if !status.message().starts_with(DECODE_ERROR) {
        return res;
    }
    let status = if compat {
        Status::internal("")
    } else {
        Status::invalid_argument(
            "Invalid request proto: an error was encountered during deserialization of the \
             request proto.",
        )
    };
    let headers = res.headers_mut();
    headers.remove("grpc-message");
    headers.remove("grpc-status-details-bin");
    // Fails only on a message that isn't a valid header value; both are ASCII.
    let _ = status.add_header(headers);
    res
}

/// The largest DATA frame a streaming response is sent in: HTTP/2's default
/// SETTINGS_MAX_FRAME_SIZE, which Go's HTTP/2 server always writes.
pub const MAX_DATA_FRAME: usize = 16 << 10;

fn small_frames(res: http::Response<tonic::body::Body>) -> http::Response<tonic::body::Body> {
    res.map(|body| {
        tonic::body::Body::new(SmallFrames {
            inner: body,
            rest: Bytes::new(),
        })
    })
}

/// Splits each data chunk of `inner` into slices of at most [`MAX_DATA_FRAME`] bytes without
/// copying. hyper hands each chunk to h2 as its own DATA frame, and h2 never merges them.
struct SmallFrames {
    inner: tonic::body::Body,
    rest: Bytes,
}

impl http_body::Body for SmallFrames {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Status>>> {
        let this = &mut *self;
        if this.rest.is_empty() {
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => this.rest = data,
                    Err(frame) => return Poll::Ready(Some(Ok(frame))),
                },
                other => return other,
            }
        }
        let n = this.rest.len().min(MAX_DATA_FRAME);
        Poll::Ready(Some(Ok(http_body::Frame::data(this.rest.split_to(n)))))
    }

    fn is_end_stream(&self) -> bool {
        self.rest.is_empty() && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        let mut hint = self.inner.size_hint();
        let rest = self.rest.len() as u64;
        if let Some(upper) = hint.upper() {
            hint.set_upper(upper + rest);
        }
        hint.set_lower(hint.lower() + rest);
        hint
    }
}

struct Method<K>(Arc<SpannerService>, K);
struct Sql;
struct Read;

impl ServerStreamingService<pb::ExecuteSqlRequest> for Method<Sql> {
    type Response = Bytes;
    type ResponseStream = ResultStream<Bytes>;
    type Future = BoxFuture<tonic::Response<Self::ResponseStream>, Status>;

    fn call(&mut self, req: tonic::Request<pb::ExecuteSqlRequest>) -> Self::Future {
        crate::timing::note(req.extensions(), || req.get_ref().sql.clone(), None);
        let svc = self.0.clone();
        Box::pin(async move {
            let out = svc.execute_streaming_sql_encoded(req.into_inner()).await?;
            Ok(tonic::Response::new(out))
        })
    }
}

impl ServerStreamingService<pb::ReadRequest> for Method<Read> {
    type Response = Bytes;
    type ResponseStream = ResultStream<Bytes>;
    type Future = BoxFuture<tonic::Response<Self::ResponseStream>, Status>;

    fn call(&mut self, req: tonic::Request<pb::ReadRequest>) -> Self::Future {
        let read = crate::services::spanner::read_label;
        crate::timing::note(req.extensions(), || read(req.get_ref()), None);
        let svc = self.0.clone();
        Box::pin(async move {
            let out = svc.streaming_read_encoded(req.into_inner()).await?;
            Ok(tonic::Response::new(out))
        })
    }
}

/// Decodes requests of type `D` with prost; writes responses, already encoded, as they are.
pub struct EncodedCodec<D>(PhantomData<fn() -> D>);

impl<D: Message + Default + Send + 'static> Codec for EncodedCodec<D> {
    type Encode = Bytes;
    type Decode = D;
    type Encoder = BytesEncoder;
    type Decoder = ProstDecoder<D>;

    fn encoder(&mut self) -> BytesEncoder {
        BytesEncoder
    }

    fn decoder(&mut self) -> ProstDecoder<D> {
        ProstDecoder(PhantomData)
    }
}

pub struct BytesEncoder;

impl Encoder for BytesEncoder {
    type Item = Bytes;
    type Error = Status;

    fn encode(&mut self, item: Bytes, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put(item);
        Ok(())
    }
}

pub struct ProstDecoder<D>(PhantomData<fn() -> D>);

impl<D: Message + Default> Decoder for ProstDecoder<D> {
    type Item = D;
    type Error = Status;

    fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<D>, Status> {
        D::decode(buf)
            .map(Some)
            .map_err(|e| Status::internal(e.to_string()))
    }
}
