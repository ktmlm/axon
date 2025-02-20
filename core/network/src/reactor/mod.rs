mod router;
mod rpc_map;

use common_apm::tracing::AxonTracer;
use std::convert::TryFrom;
use std::marker::PhantomData;

use protocol::traits::{Context, MessageCodec, MessageHandler, TrustFeedback};
use protocol::{async_trait, types::Bytes, ProtocolResult};

use crate::endpoint::{Endpoint, EndpointScheme, RpcEndpoint};
use crate::message::NetworkMessage;
use crate::rpc::RpcResponse;
use crate::traits::NetworkContext;

pub(crate) use router::{MessageRouter, RemotePeer, RouterContext};

#[async_trait]
pub trait Reactor: Send + Sync {
    async fn react(
        &self,
        context: RouterContext,
        endpoint: Endpoint,
        network_message: NetworkMessage,
    ) -> ProtocolResult<()>;
}

pub struct MessageReactor<M: MessageCodec, H: MessageHandler<Message = M>> {
    msg_handler: H,
}

pub fn generate<M: MessageCodec, H: MessageHandler<Message = M>>(h: H) -> MessageReactor<M, H> {
    MessageReactor { msg_handler: h }
}

pub fn rpc_resp<M: MessageCodec>() -> MessageReactor<M, NoopHandler<M>> {
    MessageReactor {
        msg_handler: NoopHandler::new(),
    }
}

#[async_trait]
impl<M: MessageCodec, H: MessageHandler<Message = M>> Reactor for MessageReactor<M, H> {
    async fn react(
        &self,
        context: RouterContext,
        endpoint: Endpoint,
        mut network_message: NetworkMessage,
    ) -> ProtocolResult<()> {
        let ctx = Context::new()
            .set_session_id(context.remote_peer.session_id)
            .set_remote_peer_id(context.remote_peer.peer_id.clone())
            .set_remote_connected_addr(context.remote_peer.connected_addr.clone());

        let mut ctx = match (network_message.trace_id(), network_message.span_id()) {
            (Some(trace_id), Some(span_id)) => {
                let span_state = AxonTracer::new_state(trace_id, span_id);
                AxonTracer::inject_span_state(ctx, span_state)
            }
            _ => ctx,
        };

        let session_id = context.remote_peer.session_id;
        let _feedback = match endpoint.scheme() {
            EndpointScheme::Gossip => {
                let raw_context = Bytes::from(network_message.content);
                let content = M::decode_msg(raw_context)?;
                self.msg_handler.process(ctx, content).await
            }
            EndpointScheme::RpcCall => {
                let raw_context = Bytes::from(network_message.content);
                let content = M::decode_msg(raw_context)?;
                let rpc_endpoint = RpcEndpoint::try_from(endpoint)?;

                let ctx = ctx.set_rpc_id(rpc_endpoint.rpc_id().value());
                self.msg_handler.process(ctx, content).await
            }
            EndpointScheme::RpcResponse => {
                let content = {
                    if !network_message.content.is_empty() {
                        let raw = network_message.content.split_off(1);

                        if network_message.content[0] == 0 {
                            RpcResponse::Success(Bytes::from(raw))
                        } else {
                            RpcResponse::Error(String::from_utf8_lossy(&raw).to_string())
                        }
                    } else {
                        RpcResponse::Error("empty message".to_string())
                    }
                };
                let rpc_endpoint = RpcEndpoint::try_from(endpoint)?;
                let rpc_id = rpc_endpoint.rpc_id().value();

                if !context.rpc_map.contains(session_id, rpc_id) {
                    let full_url = rpc_endpoint.endpoint().full_url();

                    log::warn!(
                        "rpc to {} from {} not found, maybe timeout",
                        full_url,
                        context.remote_peer
                    );
                    return Ok(());
                }

                let rpc_id = rpc_endpoint.rpc_id().value();
                let resp_tx = context.rpc_map.take::<RpcResponse>(session_id, rpc_id)?;
                if resp_tx.send(content).is_err() {
                    let end = rpc_endpoint.endpoint().full_url();
                    log::warn!("network: reactor: {} rpc dropped on {}", session_id, end);
                }

                return Ok(());
            }
        };

        // context.report_feedback(feedback);
        Ok(())
    }
}

#[derive(Debug)]
pub struct NoopHandler<M> {
    pin_m: PhantomData<fn() -> M>,
}

impl<M> NoopHandler<M>
where
    M: MessageCodec,
{
    pub fn new() -> Self {
        NoopHandler { pin_m: PhantomData }
    }
}

#[async_trait]
impl<M> MessageHandler for NoopHandler<M>
where
    M: MessageCodec,
{
    type Message = M;

    async fn process(&self, _: Context, _: Self::Message) -> TrustFeedback {
        TrustFeedback::Neutral
    }
}
