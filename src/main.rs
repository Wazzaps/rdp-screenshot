//! Example of utilizing IronRDP in a blocking, synchronous fashion.
//!
//! This example showcases the use of IronRDP in a blocking manner. It
//! demonstrates how to create a basic RDP client with just a few hundred lines
//! of code by leveraging the IronRDP crates suite.
//!
//! In this basic client implementation, the client establishes a connection
//! with the destination server, decodes incoming graphics updates, and saves the
//! resulting output as a PNG image file on the disk.
//!
//! # Usage example
//!
//! ```shell
//! cargo run --example=screenshot -- --host <HOSTNAME> -u <USERNAME> -p <PASSWORD> -o out.png
//! ```

#![allow(unused_crate_dependencies)] // false positives because there is both a library and a binary
#![allow(clippy::print_stdout)]

use core::time::Duration;
use std::io::Write as _;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::Context as _;
use clap::Parser;
use connector::Credentials;
use ironrdp::connector;
use ironrdp::connector::ConnectionResult;
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use sspi::network_client::reqwest_network_client::ReqwestNetworkClient;
use tokio_rustls::rustls;
use tracing::{debug, info, trace};

#[derive(Debug, Parser)]
#[command(about = "Basic RDP screenshot CLI based on IronRDP")]
struct Args {
    /// RDP server hostname
    #[arg(long)]
    host: String,

    /// RDP server port
    #[arg(long, default_value_t = 3389)]
    port: u16,

    /// Username for authentication
    #[arg(short, long)]
    username: String,

    /// Password for authentication
    #[arg(short, long)]
    password: String,

    /// Output PNG file path
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Domain for authentication
    #[arg(short, long)]
    domain: Option<String>,

    /// Keep the connection open and save a screenshot every 5 seconds
    #[arg(long)]
    keep_open: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    setup_logging()?;

    info!(args.host, args.port, args.username, ?args.output, args.domain, args.keep_open, "run");
    run(
        args.host,
        args.port,
        args.username,
        args.password,
        args.output,
        args.domain,
        args.keep_open,
    )
}

fn setup_logging() -> anyhow::Result<()> {
    use tracing::metadata::LevelFilter;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::prelude::*;

    let fmt_layer = tracing_subscriber::fmt::layer().compact();

    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("IRONRDP_LOG")
        .from_env_lossy();

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(env_filter)
        .try_init()
        .context("failed to set tracing global subscriber")?;

    Ok(())
}

fn run(
    server_name: String,
    port: u16,
    username: String,
    password: String,
    output: Option<PathBuf>,
    domain: Option<String>,
    keep_open: bool,
) -> anyhow::Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::Relaxed);
    })
    .context("set Ctrl+C handler")?;

    let config = build_config(username, password, domain);

    let (connection_result, framed) = connect(config, server_name, port).context("connect")?;

    let mut image = DecodedImage::new(
        ironrdp_graphics::image_processing::PixelFormat::RgbA32,
        connection_result.desktop_size.width,
        connection_result.desktop_size.height,
    );

    if keep_open {
        active_stage_keep_open(
            connection_result,
            framed,
            &mut image,
            output.as_deref(),
            &running,
        )
        .context("active stage")?;
    } else {
        active_stage(connection_result, framed, &mut image).context("active stage")?;

        if let Some(output) = output {
            save_image(&image, &output)?;
        }
    }

    Ok(())
}

fn build_config(username: String, password: String, domain: Option<String>) -> connector::Config {
    connector::Config {
        credentials: Credentials::UsernamePassword { username, password },
        domain,
        enable_tls: false, // This example does not expose any frontend.
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize {
            width: 1280,
            height: 1024,
        },
        bitmap: None,
        client_build: 0,
        client_name: "rdp-screenshot".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),

        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(target_os = "ios")]
        platform: MajorPlatformType::IOS,
        #[cfg(target_os = "linux")]
        platform: MajorPlatformType::UNIX,
        #[cfg(target_os = "android")]
        platform: MajorPlatformType::ANDROID,
        #[cfg(target_os = "freebsd")]
        platform: MajorPlatformType::UNIX,
        #[cfg(target_os = "dragonfly")]
        platform: MajorPlatformType::UNIX,
        #[cfg(target_os = "openbsd")]
        platform: MajorPlatformType::UNIX,
        #[cfg(target_os = "netbsd")]
        platform: MajorPlatformType::UNIX,

        enable_server_pointer: false, // Disable custom pointers (there is no user interaction anyway).
        request_data: None,
        autologon: true,
        enable_audio_playback: false,
        pointer_software_rendering: true,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
    }
}

type UpgradedFramed =
    ironrdp_blocking::Framed<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

fn connect(
    config: connector::Config,
    server_name: String,
    port: u16,
) -> anyhow::Result<(ConnectionResult, UpgradedFramed)> {
    let server_addr = lookup_addr(&server_name, port).context("lookup addr")?;

    info!(%server_addr, "Looked up server address");

    let tcp_stream = TcpStream::connect(server_addr).context("TCP connect")?;

    // Sets the read timeout for the TCP stream so we can break out of the
    // infinite loop during the active stage once there is no more activity.
    tcp_stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set_read_timeout call failed");

    let client_addr = tcp_stream
        .local_addr()
        .context("get socket local address")?;

    let mut framed = ironrdp_blocking::Framed::new(tcp_stream);

    let mut connector = connector::ClientConnector::new(config, client_addr);

    let should_upgrade =
        ironrdp_blocking::connect_begin(&mut framed, &mut connector).context("begin connection")?;

    debug!("TLS upgrade");

    // Ensure there is no leftover
    let initial_stream = framed.into_inner_no_leftover();

    let (upgraded_stream, server_public_key) =
        tls_upgrade(initial_stream, server_name.clone()).context("TLS upgrade")?;

    let upgraded = ironrdp_blocking::mark_as_upgraded(should_upgrade, &mut connector);

    let mut upgraded_framed = ironrdp_blocking::Framed::new(upgraded_stream);

    let mut network_client = ReqwestNetworkClient;
    let connection_result = ironrdp_blocking::connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut network_client,
        server_name.into(),
        server_public_key,
        None,
    )
    .context("finalize connection")?;

    Ok((connection_result, upgraded_framed))
}

fn active_stage(
    connection_result: ConnectionResult,
    mut framed: UpgradedFramed,
    image: &mut DecodedImage,
) -> anyhow::Result<()> {
    let mut active_stage = ActiveStage::new(connection_result);

    'outer: loop {
        let (action, payload) = match framed.read_pdu() {
            Ok((action, payload)) => (action, payload),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break 'outer,
            Err(e) => return Err(anyhow::Error::new(e).context("read frame")),
        };

        trace!(?action, frame_length = payload.len(), "Frame received");

        let outputs = active_stage.process(image, action, &payload)?;

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => {
                    framed.write_all(&frame).context("write response")?
                }
                ActiveStageOutput::Terminate(_) => break 'outer,
                _ => {}
            }
        }
    }

    Ok(())
}

fn active_stage_keep_open(
    connection_result: ConnectionResult,
    mut framed: UpgradedFramed,
    image: &mut DecodedImage,
    output: Option<&Path>,
    running: &AtomicBool,
) -> anyhow::Result<()> {
    let mut active_stage = ActiveStage::new(connection_result);
    let mut screenshot_counter: u64 = 0;
    let mut last_save = Instant::now();

    loop {
        if !running.load(Ordering::Relaxed) {
            info!("Interrupted, shutting down");
            break;
        }

        let (action, payload) = match framed.read_pdu() {
            Ok((action, payload)) => (action, payload),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if last_save.elapsed() >= Duration::from_secs(5) {
                    if let Some(output) = output {
                        screenshot_counter += 1;
                        let path = numbered_output_path(output, screenshot_counter);
                        save_image(image, &path)?;
                        info!(%screenshot_counter, path = %path.display(), "Saved screenshot");
                    }
                    last_save = Instant::now();
                }
                continue;
            }
            Err(e) => return Err(anyhow::Error::new(e).context("read frame")),
        };

        trace!(?action, frame_length = payload.len(), "Frame received");

        let outputs = active_stage.process(image, action, &payload)?;

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => {
                    framed.write_all(&frame).context("write response")?
                }
                ActiveStageOutput::Terminate(_) => {
                    info!("Server terminated connection");
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn save_image(image: &DecodedImage, path: &Path) -> anyhow::Result<()> {
    let img: image::ImageBuffer<image::Rgba<u8>, _> = image::ImageBuffer::from_raw(
        u32::from(image.width()),
        u32::from(image.height()),
        image.data(),
    )
    .context("invalid image")?;

    img.save(path).context("save image to disk")?;
    Ok(())
}

fn numbered_output_path(base: &Path, n: u64) -> PathBuf {
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ext = base.extension().and_then(|s| s.to_str());
    let parent = base.parent().unwrap_or(Path::new(""));
    match ext {
        Some(ext) => parent.join(format!("{stem}_{n}.{ext}")),
        None => parent.join(format!("{stem}_{n}")),
    }
}

fn lookup_addr(hostname: &str, port: u16) -> anyhow::Result<core::net::SocketAddr> {
    use std::net::ToSocketAddrs as _;
    let addr = (hostname, port)
        .to_socket_addrs()?
        .next()
        .context("socket address not found")?;
    Ok(addr)
}

fn tls_upgrade(
    stream: TcpStream,
    server_name: String,
) -> anyhow::Result<(
    rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    Vec<u8>,
)> {
    let mut config = rustls::client::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(danger::NoCertificateVerification))
        .with_no_client_auth();

    // This adds support for the SSLKEYLOGFILE env variable (https://wiki.wireshark.org/TLS#using-the-pre-master-secret)
    config.key_log = std::sync::Arc::new(rustls::KeyLogFile::new());

    // Disable TLS resumption because it’s not supported by some services such as CredSSP.
    //
    // > The CredSSP Protocol does not extend the TLS wire protocol. TLS session resumption is not supported.
    //
    // source: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cssp/385a7489-d46b-464c-b224-f7340e308a5c
    config.resumption = rustls::client::Resumption::disabled();

    let config = std::sync::Arc::new(config);

    let server_name = server_name.try_into()?;

    let client = rustls::ClientConnection::new(config, server_name)?;

    let mut tls_stream = rustls::StreamOwned::new(client, stream);

    // We need to flush in order to ensure the TLS handshake is moving forward. Without flushing,
    // it’s likely the peer certificate is not yet received a this point.
    tls_stream.flush()?;

    let cert = tls_stream
        .conn
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .context("peer certificate is missing")?;

    let server_public_key = extract_tls_server_public_key(cert)?;

    Ok((tls_stream, server_public_key))
}

fn extract_tls_server_public_key(cert: &[u8]) -> anyhow::Result<Vec<u8>> {
    use x509_cert::der::Decode as _;

    let cert = x509_cert::Certificate::from_der(cert)?;

    debug!(%cert.tbs_certificate.subject);

    let server_public_key = cert
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .context("subject public key BIT STRING is not aligned")?
        .to_owned();

    Ok(server_public_key)
}

mod danger {
    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::{DigitallySignedStruct, Error, SignatureScheme, pki_types};

    #[derive(Debug)]
    pub(super) struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _: &pki_types::CertificateDer<'_>,
            _: &[pki_types::CertificateDer<'_>],
            _: &pki_types::ServerName<'_>,
            _: &[u8],
            _: pki_types::UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
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
}
