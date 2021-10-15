use std::{
    convert::{TryFrom, TryInto},
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use futures::{future::BoxFuture, Future, Stream};
use log::trace;
use parking_lot::RwLock;
use rasn_ldap::{
    AuthenticationChoice, BindRequest, LdapMessage, LdapResult, ProtocolOp, ResultCode, SearchRequest,
    SearchResultEntry, UnbindRequest,
};

use crate::conn::MessageStream;
use crate::{
    channel::TlsOptions,
    conn::LdapConnection,
    controls::{SimplePagedResultsControl, PAGED_CONTROL_OID},
    error::Error,
};

pub type Result<T> = std::result::Result<T, Error>;

pub struct LdapClientBuilder {
    address: String,
    port: u16,
    tls_options: TlsOptions,
}

impl LdapClientBuilder {
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn tls_options(mut self, options: TlsOptions) -> Self {
        self.tls_options = options;
        self
    }

    pub async fn build_and_connect(self) -> Result<LdapClient> {
        LdapClient::connect(self.address, self.port, self.tls_options).await
    }
}

#[derive(Clone)]
pub struct LdapClient {
    connection: LdapConnection,
    id_counter: Arc<AtomicU32>,
}

impl LdapClient {
    pub fn builder<A: AsRef<str>>(address: A) -> LdapClientBuilder {
        LdapClientBuilder {
            address: address.as_ref().to_owned(),
            port: 389,
            tls_options: TlsOptions::plain(),
        }
    }

    pub async fn connect<A>(address: A, port: u16, tls_options: TlsOptions) -> Result<Self>
    where
        A: AsRef<str>,
    {
        let connection = LdapConnection::connect(address, port, tls_options).await?;
        Ok(Self {
            connection,
            id_counter: Arc::new(AtomicU32::new(2)), // 1 is used by STARTTLS
        })
    }

    fn new_id(&mut self) -> u32 {
        self.id_counter.fetch_add(1, Ordering::SeqCst)
    }

    fn check_result(&self, result: LdapResult) -> Result<()> {
        if result.result_code == ResultCode::Success {
            Ok(())
        } else {
            Err(Error::OperationFailed(result.into()))
        }
    }

    pub async fn simple_bind<U, P>(&mut self, username: U, password: P) -> Result<()>
    where
        U: AsRef<str>,
        P: AsRef<str>,
    {
        let id = self.new_id();

        let auth_choice = AuthenticationChoice::Simple(password.as_ref().to_owned().into());
        let req = BindRequest::new(3, username.as_ref().to_owned().into(), auth_choice);
        let msg = LdapMessage::new(id, ProtocolOp::BindRequest(req));

        trace!("Sending message: {:?}", msg);
        let item = self.connection.send_recv(msg).await?;
        trace!("Received message: {:?}", item);

        match item.protocol_op {
            ProtocolOp::BindResponse(resp) => Ok(self.check_result(LdapResult::new(
                resp.result_code,
                resp.matched_dn,
                resp.diagnostic_message,
            ))?),
            _ => Err(Error::InvalidResponse),
        }
    }

    pub async fn unbind(&mut self) -> Result<()> {
        let id = self.new_id();

        let msg = LdapMessage::new(id, ProtocolOp::UnbindRequest(UnbindRequest));
        self.connection.send(msg).await?;

        Ok(())
    }

    pub async fn search(&mut self, request: SearchRequest) -> Result<SearchEntries> {
        let id = self.new_id();

        let msg = LdapMessage::new(id, ProtocolOp::SearchRequest(request));
        let stream = self.connection.send_recv_stream(msg).await?;

        Ok(SearchEntries {
            inner: stream,
            control: None,
        })
    }

    pub fn search_paged(&mut self, request: SearchRequest, page_size: u32) -> Pages {
        Pages {
            control: Arc::new(RwLock::new(SimplePagedResultsControl::new(page_size))),
            client: self.clone(),
            request,
            page_size,
            inner: None,
        }
    }
}

pub struct Pages {
    control: Arc<RwLock<SimplePagedResultsControl>>,
    client: LdapClient,
    request: SearchRequest,
    page_size: u32,
    inner: Option<BoxFuture<'static, Result<SearchEntries>>>,
}

impl Stream for Pages {
    type Item = Result<SearchEntries>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.control.read().has_entries() {
            return Poll::Ready(None);
        }

        if self.inner.is_none() {
            let mut client = self.client.clone();
            let request = self.request.clone();
            let control_ref = self.control.clone();
            let page_size = self.page_size;

            let fut = async move {
                let id = client.new_id();

                let mut msg = LdapMessage::new(id, ProtocolOp::SearchRequest(request));
                msg.controls = Some(vec![control_ref.read().clone().with_size(page_size).try_into()?]);

                let stream = client.connection.send_recv_stream(msg).await?;
                Ok(SearchEntries {
                    inner: stream,
                    control: Some(control_ref),
                })
            };
            self.inner = Some(Box::pin(fut));
        }

        match Pin::new(self.inner.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Some(Err(err))),
            Poll::Ready(Ok(entries)) => {
                self.inner = None;
                Poll::Ready(Some(Ok(entries)))
            }
        }
    }
}

pub struct SearchEntries {
    inner: MessageStream,
    control: Option<Arc<RwLock<SimplePagedResultsControl>>>,
}

impl Stream for SearchEntries {
    type Item = Result<SearchResultEntry>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Some(Err(Error::ConnectionClosed))),
                Poll::Ready(Some(msg)) => {
                    match msg.protocol_op {
                        ProtocolOp::SearchResEntry(item) => return Poll::Ready(Some(Ok(item))),
                        ProtocolOp::SearchResRef(_) => {}
                        ProtocolOp::SearchResDone(done) => {
                            return if done.0.result_code == ResultCode::Success {
                                if let Some(ref control_ref) = self.control {
                                    let page_control = msg.controls.and_then(|controls| {
                                        controls
                                            .into_iter()
                                            .find(|c| c.control_type == PAGED_CONTROL_OID)
                                            .map(|c| SimplePagedResultsControl::try_from(c).ok())
                                            .flatten()
                                    });

                                    if let Some(page_control) = page_control {
                                        *control_ref.write() = page_control;
                                        Poll::Ready(None)
                                    } else {
                                        Poll::Ready(Some(Err(Error::InvalidResponse)))
                                    }
                                } else {
                                    Poll::Ready(None)
                                }
                            } else {
                                Poll::Ready(Some(Err(Error::OperationFailed(done.0.into()))))
                            }
                        }
                        _ => return Poll::Ready(Some(Err(Error::InvalidResponse))),
                    }
                },
            }
        }
    }
}
