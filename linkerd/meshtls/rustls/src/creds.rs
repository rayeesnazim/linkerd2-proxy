mod receiver;
mod store;

pub use self::{receiver::Receiver, store::Store};
use linkerd_error::Result;
use linkerd_identity as id;
use ring::{error::KeyRejected, signature::EcdsaKeyPair};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;
use tokio_rustls::rustls;
use tracing::warn;

#[derive(Debug, Error)]
#[error(transparent)]
pub struct InvalidKey(KeyRejected);

#[derive(Debug, Error)]
#[error("invalid trust roots")]
pub struct InvalidTrustRoots(());

pub fn watch(
    identity: id::Name,
    roots_pem: &str,
    key_pkcs8: &[u8],
    csr: &[u8],
) -> Result<(Store, Receiver)> {
    let mut roots = rustls::RootCertStore::empty();
    let certs = match rustls_pemfile::certs(&mut std::io::Cursor::new(roots_pem)) {
        Err(error) => {
            warn!(%error, "invalid trust anchors file");
            return Err(error.into());
        }
        Ok(certs) if certs.is_empty() => {
            warn!("no valid certs in trust anchors file");
            return Err("no trust roots in PEM file".into());
        }
        Ok(certs) => certs,
    };

    let (added, skipped) = roots.add_parsable_certificates(&certs[..]);
    if skipped != 0 {
        warn!("Skipped {} invalid trust anchors", skipped);
    }
    if added == 0 {
        return Err("no trust roots loaded".into());
    }

    let key = EcdsaKeyPair::from_pkcs8(params::SIGNATURE_ALG_RING_SIGNING, key_pkcs8)
        .map_err(InvalidKey)?;

    // XXX: Rustls's built-in verifiers don't let us tweak things as fully as we'd like (e.g.
    // controlling the set of trusted signature algorithms), but they provide good enough
    // defaults for now.
    // TODO: lock down the verification further.
    let server_cert_verifier = Arc::new(rustls::client::WebPkiVerifier::new(
        roots.clone(),
        None, // no certificate transparency policy
    ));

    let (client_tx, client_rx) = {
        // Since we don't have a certificate yet, build a client configuration
        // that doesn't attempt client authentication. Once we get a
        // certificate, the `Store` will publish a new configuration with a
        // client certificate resolver.
        let mut c =
            store::client_config_builder(server_cert_verifier.clone()).with_no_client_auth();

        // Disable session resumption for the time-being until resumption is
        // more tested.
        c.resumption = rustls::client::Resumption::disabled();

        watch::channel(Arc::new(c))
    };
    let (server_tx, server_rx) = {
        // Since we don't have a certificate yet, use an empty cert resolver so
        // that handshaking always fails. Once we get a certificate, the `Store`
        // will publish a new configuration with a server certificate resolver.
        let empty_resolver = Arc::new(rustls::server::ResolvesServerCertUsingSni::new());
        watch::channel(store::server_config(roots.clone(), empty_resolver))
    };

    let rx = Receiver::new(identity.clone(), client_rx, server_rx);
    let store = Store::new(
        roots,
        server_cert_verifier,
        key,
        csr,
        identity,
        client_tx,
        server_tx,
    );

    Ok((store, rx))
}

#[cfg(feature = "test-util")]
pub fn for_test(ent: &linkerd_tls_test_util::Entity) -> (Store, Receiver) {
    watch(
        ent.name.parse().expect("name must be valid"),
        std::str::from_utf8(ent.trust_anchors).expect("roots must be PEM"),
        ent.key,
        b"fake CSR",
    )
    .expect("credentials must be valid")
}

#[cfg(feature = "test-util")]
pub fn default_for_test() -> (Store, Receiver) {
    for_test(&linkerd_tls_test_util::FOO_NS1)
}

mod params {
    use tokio_rustls::rustls;

    // These must be kept in sync:
    pub static SIGNATURE_ALG_RING_SIGNING: &ring::signature::EcdsaSigningAlgorithm =
        &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
    pub const SIGNATURE_ALG_RUSTLS_SCHEME: rustls::SignatureScheme =
        rustls::SignatureScheme::ECDSA_NISTP256_SHA256;
    pub const SIGNATURE_ALG_RUSTLS_ALGORITHM: rustls::SignatureAlgorithm =
        rustls::SignatureAlgorithm::ECDSA;
    pub static TLS_VERSIONS: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];
    pub static TLS_SUPPORTED_CIPHERSUITES: &[rustls::SupportedCipherSuite] =
        &[rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256];
}
