use rust_tls_duplex_stream::RustTlsDuplexStream;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, SignatureScheme};
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug)]
pub struct VeryGoodVerifier();

impl ServerCertVerifier for VeryGoodVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

#[test]
fn main_client() {
    rustls_graviola::default_provider()
        .install_default()
        .unwrap();

    let socket = std::net::TcpStream::connect("browserleaks.com:443").unwrap();
    let config = Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(VeryGoodVerifier()))
            .with_no_client_auth(),
    );

    let dns_name = ServerName::try_from("browserleaks.com").expect("invalid DNS name");
    let client = rustls::ClientConnection::new(config, dns_name).unwrap();
    let wrapper = RustTlsDuplexStream::new_unpooled(client, socket.try_clone().unwrap(), socket)
        .expect("error spawning threads");

    //let mut stream = rustls::Stream::new(&mut client, &mut socket); // Create stream
    // Instead of writing to the client, you write to the stream
    let now = Instant::now();
    wrapper
        .write(b"GET / HTTP/1.1\r\nHost: browserleaks.com\r\nConnection: close\r\n\r\n")
        .unwrap();
    wrapper.flush().unwrap();
    let mut plaintext = Vec::new();
    wrapper.read_to_end(&mut plaintext).unwrap();
    let uw = now.elapsed().as_millis();
    io::stdout().write_all(&plaintext).unwrap();
    println!("{}", uw);
}
