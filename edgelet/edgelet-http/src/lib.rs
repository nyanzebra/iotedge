// Copyright (c) Microsoft. All rights reserved.

#![deny(rust_2018_idioms, warnings)]
#![deny(clippy::all, clippy::pedantic)]
#![allow(
    clippy::default_trait_access,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::pub_enum_variant_names,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::use_self
)]

use std::fmt;
use std::fmt::{Debug, Formatter};
#[cfg(target_os = "linux")]
use std::net;
#[cfg(target_os = "linux")]
use std::os::unix::io::FromRawFd;
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(unix)]
use std::sync::Arc;

use failure::{Fail, ResultExt};
use futures::{future, Future, Poll, Stream};
use hyper::server::conn::Http;
use hyper::service::{NewService, Service};
use hyper::{Body, Response};
use log::{debug, error, Level};
use native_tls::Identity;
#[cfg(unix)]
use openssl::pkcs12::Pkcs12;
use openssl::pkey::PKey;
use openssl::stack::Stack;
use openssl::x509::X509;
#[cfg(target_os = "linux")]
use systemd::Socket;
use tokio::net::TcpListener;
#[cfg(target_os = "linux")]
use tokio_uds::UnixListener;
use url::Url;

use edgelet_core::crypto::{Certificate, KeyBytes, PrivateKey};
use edgelet_core::{UrlExt, UNIX_SCHEME};
use edgelet_utils::log_failure;

pub mod authentication;
pub mod authorization;
pub mod client;
pub mod error;
pub mod logging;
mod pid;
pub mod route;
mod unix;
mod util;
mod version;

pub use error::{BindListenerType, Error, ErrorKind, InvalidUrlReason};
pub use pid::Pid;
pub use util::proxy::MaybeProxyClient;
pub use util::UrlConnector;
pub use version::{Version, API_VERSION};

use crate::pid::PidService;
use crate::util::incoming::Incoming;

const HTTP_SCHEME: &str = "http";
const TCP_SCHEME: &str = "tcp";
#[cfg(target_os = "linux")]
const FD_SCHEME: &str = "fd";

#[derive(Clone, Copy)]
pub enum ConcurrencyThrottling {
    Limited(u32),
    NoLimit,
}

#[derive(Clone)]
pub struct PemCertificate {
    cert: Vec<u8>,
    key: Option<Vec<u8>>,
    username: Option<String>,
    password: Option<String>,
}

impl PemCertificate {
    pub fn new(
        cert: Vec<u8>,
        key: Option<Vec<u8>>,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        PemCertificate {
            cert,
            key,
            username,
            password,
        }
    }

    pub fn get_certificate(&self) -> &[u8] {
        &self.cert
    }

    pub fn from<C>(id_cert: &C) -> Result<Self, Error>
    where
        C: Certificate,
    {
        let cert = id_cert
            .pem()
            .map(|cert_buffer| cert_buffer.as_ref().to_owned())
            .context(ErrorKind::IdentityCertificate)?;

        let key = match id_cert.get_private_key() {
            Ok(Some(PrivateKey::Ref(ref_))) => Some(ref_.into_bytes()),
            Ok(Some(PrivateKey::Key(KeyBytes::Pem(buffer)))) => Some(buffer.as_ref().to_vec()),
            Ok(None) => None,
            Err(_err) => return Err(Error::from(ErrorKind::IdentityPrivateKey)),
        };

        Ok(PemCertificate::new(cert, key, None, None))
    }

    pub fn get_identity(&self) -> Result<Identity, Error> {
        let mut certs = X509::stack_from_pem(&self.cert).context(ErrorKind::IdentityCertificate)?;

        // the first cert is the identity cert and the other certs are part of the CA
        // chain; we skip the server cert and build an OpenSSL cert stack with the
        // other certs
        let mut ca_certs = Stack::new().context(ErrorKind::IdentityCertificate)?;
        for cert in certs.drain(1..) {
            ca_certs
                .push(cert)
                .context(ErrorKind::IdentityCertificate)?;
        }

        let key = match &self.key {
            Some(k) => PKey::private_key_from_pem(&k)
                .with_context(|err| ErrorKind::IdentityPrivateKeyRead(err.to_string())),
            None => return Err(Error::from(ErrorKind::IdentityPrivateKey)),
        }?;

        let identity_cert = &certs[0];

        let mut builder = Pkcs12::builder();
        builder.ca(ca_certs);
        let pkcs_certs = builder
            .build(
                self.password.as_ref().map_or("", String::as_str),
                self.username.as_ref().map_or("", String::as_str),
                &key,
                &identity_cert,
            )
            .context(ErrorKind::IdentityCertificate)?;

        let der = pkcs_certs
            .to_der()
            .context(ErrorKind::IdentityCertificate)?;

        let identity = Identity::from_pkcs12(&der, "")
            .with_context(|err| ErrorKind::PKCS12Identity(err.to_string()))?;

        Ok(identity)
    }
}

impl Debug for PemCertificate {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        // do not print either the username, password or private key
        write!(f, "Certificate: {:?}", self.cert)
    }
}

pub trait IntoResponse {
    fn into_response(self) -> Response<Body>;
}

impl IntoResponse for Response<Body> {
    fn into_response(self) -> Response<Body> {
        self
    }
}

pub struct Run(Box<dyn Future<Item = (), Error = Error> + Send + 'static>);

impl Future for Run {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

pub struct Server<S> {
    protocol: Http,
    new_service: S,
    incoming: Incoming,
}

impl<S> Server<S>
where
    S: NewService<ReqBody = Body, ResBody = Body> + Send + 'static,
    S::Future: Send + 'static,
    S::Service: Send + 'static,
    S::InitError: Fail,
    <S::Service as Service>::Future: Send + 'static,
{
    pub fn run(self) -> Run {
        self.run_until(future::empty(), ConcurrencyThrottling::NoLimit)
    }

    pub fn run_until<F>(self, shutdown_signal: F, concurrency: ConcurrencyThrottling) -> Run
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        let Server {
            protocol,
            new_service,
            incoming,
        } = self;

        let protocol = Arc::new(protocol);

        let lock = if let ConcurrencyThrottling::Limited(limit) = concurrency {
            Some((Arc::new(AtomicU32::new(limit)), limit))
        } else {
            None
        };

        let srv = incoming.for_each(move |(socket, addr)| {

            if let Some((lock, limit)) = lock.clone() {
                let limit_reached = lock.fetch_update( Ordering::AcqRel,
                    Ordering::Acquire,
                    |current| current.checked_sub(1),
                    ).is_err();

                if limit_reached {
                    error!("Maximum concurrency reached, {} simultaneous connections, dropping the connection request", limit);
                    // Return Ok so the stream is not stopped.
                    return Ok(());
                }
            }

            let protocol = protocol.clone();
            debug!("accepted new connection ({})", addr);
            let pid = socket.pid()?;
            let fut = new_service
                .new_service()
                .then({
                    let lock = lock.clone();
                    move |srv| match srv {
                    Ok(srv) => Ok((srv, addr)),
                    Err(err) => {
                        error!("server connection error: ({})", addr);
                        if let Some((lock, _)) = lock {
                            lock.fetch_add(1, Ordering::AcqRel);
                        }
                        log_failure(Level::Error, &err);
                        Err(())
                    }
                }})
                .and_then({
                    let lock = lock.clone();
                    move |(srv, addr)| {
                    let service = PidService::new(pid, srv);
                    protocol
                        .serve_connection(socket, service)
                        .then(move |result| {
                            if let Some((lock, _)) = lock {
                                lock.fetch_add(1, Ordering::AcqRel);
                            }
                            match result {
                            Ok(_) => Ok(()),
                            Err(err) => {
                                error!("server connection error: ({})", addr);
                                log_failure(Level::Error, &err);
                                Err(())
                            }
                        }})
                }});
            tokio::spawn(fut);
            Ok(())
        });

        // We don't care if the shut_down signal errors.
        // Swallow the error.
        let shutdown_signal = shutdown_signal.then(|_| Ok(()));

        // Main execution
        // Use select to wait for either `incoming` or `f` to resolve.
        let main_execution = shutdown_signal
            .select(srv)
            .then(move |result| match result {
                Ok(((), _other)) => Ok(()),
                Err((e, _other)) => Err(Error::from(e.context(ErrorKind::ServiceError))),
            });

        Run(Box::new(main_execution))
    }

    pub fn port(&self) -> Option<u16> {
        match &self.incoming {
            Incoming::Tcp(listener) => listener.local_addr().ok().map(|addr| addr.port()),
            Incoming::Unix(_) => None,
        }
    }
}

pub trait HyperExt {
    fn bind_url<S>(
        &self,
        url: Url,
        new_service: S,
        unix_socket_permission: u32,
    ) -> Result<Server<S>, Error>
    where
        S: NewService<ReqBody = Body>;
}

// This variable is used on Unix but not Windows
impl HyperExt for Http {
    #[cfg_attr(not(unix), allow(unused_variables))]
    fn bind_url<S>(
        &self,
        url: Url,
        new_service: S,
        unix_socket_permission: u32,
    ) -> Result<Server<S>, Error>
    where
        S: NewService<ReqBody = Body>,
    {
        let incoming = match url.scheme() {
            HTTP_SCHEME | TCP_SCHEME => {
                let addr = url
                    .socket_addrs(|| None)
                    .context(ErrorKind::InvalidUrl(url.to_string()))?;
                let addr = addr.get(0);
                let addr = addr.ok_or_else(|| {
                    ErrorKind::InvalidUrlWithReason(url.to_string(), InvalidUrlReason::NoAddress)
                })?;

                let listener = TcpListener::bind(&addr)
                    .with_context(|_| ErrorKind::BindListener(BindListenerType::Address(*addr)))?;
                Incoming::Tcp(listener)
            }
            UNIX_SCHEME => {
                let path = url
                    .to_uds_file_path()
                    .map_err(|_| ErrorKind::InvalidUrl(url.to_string()))?;
                unix::listener(path, unix_socket_permission)?
            }
            #[cfg(target_os = "linux")]
            FD_SCHEME => {
                let host = url.host_str().ok_or_else(|| {
                    ErrorKind::InvalidUrlWithReason(url.to_string(), InvalidUrlReason::NoHost)
                })?;

                // Try to parse the host as an FD number, then as an FD name
                let socket = host
                    .parse()
                    .map_err(|_| ())
                    .and_then(|num| systemd::listener(num).map_err(|_| ()))
                    .or_else(|_| systemd::listener_name(host))
                    .with_context(|_| {
                        ErrorKind::InvalidUrlWithReason(
                            url.to_string(),
                            InvalidUrlReason::FdNeitherNumberNorName,
                        )
                    })?;

                match socket {
                    Socket::Inet(fd, _addr) => {
                        let l = unsafe { net::TcpListener::from_raw_fd(fd) };
                        Incoming::Tcp(
                            TcpListener::from_std(l, &Default::default()).with_context(|_| {
                                ErrorKind::BindListener(BindListenerType::Fd(fd))
                            })?,
                        )
                    }
                    Socket::Unix(fd) => {
                        let l = unsafe { ::std::os::unix::net::UnixListener::from_raw_fd(fd) };
                        Incoming::Unix(
                            UnixListener::from_std(l, &Default::default()).with_context(|_| {
                                ErrorKind::BindListener(BindListenerType::Fd(fd))
                            })?,
                        )
                    }
                    Socket::Unknown => {
                        return Err(ErrorKind::InvalidUrlWithReason(
                            url.to_string(),
                            InvalidUrlReason::UnrecognizedSocket,
                        )
                        .into())
                    }
                }
            }
            _ => {
                return Err(Error::from(ErrorKind::InvalidUrlWithReason(
                    url.to_string(),
                    InvalidUrlReason::InvalidScheme,
                )))
            }
        };

        Ok(Server {
            protocol: self.clone(),
            new_service,
            incoming,
        })
    }
}
