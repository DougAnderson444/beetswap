//! The server node portion of the interop
#![cfg(not(target_arch = "wasm32"))]

use std::net::{Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{http, Router};
use blockstore::InMemoryBlockstore;
use cid::Cid;
use futures_util::stream::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::request_response::Message;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, SwarmBuilder};
use test_utils::PeerRequest;
use thirtyfour::{prelude::*, PageLoadStrategy};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Child;
use tokio::sync::watch;
use tower_http::cors::{self, CorsLayer};

const CID_SIZE: usize = 64;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("interop_tests_native=debug,beetswap=debug")
        .try_init();

    let blockstore: InMemoryBlockstore<CID_SIZE> = InMemoryBlockstore::new();

    let mut swarm = SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_other_transport(|key| {
            Ok(libp2p_webrtc::tokio::Transport::new(
                key.clone(),
                libp2p_webrtc::tokio::Certificate::generate(&mut rand::thread_rng())?,
            ))
        })?
        .with_behaviour(|key| test_utils::new_behaviour(key, blockstore))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(32_212_254u64)))
        .build();

    let address_webrtc = Multiaddr::from(Ipv4Addr::UNSPECIFIED)
        .with(Protocol::Udp(0))
        .with(Protocol::WebRTCDirect);

    swarm.listen_on(address_webrtc).unwrap();

    let address = loop {
        if let SwarmEvent::NewListenAddr { address, .. } = swarm.next().await.unwrap() {
            tracing::info!(%address, "RXD Address");
            match address.iter().next() {
                Some(Protocol::Ip6(ip6)) => {
                    // Only add our globally available IPv6 addresses to the external addresses list.
                    if !ip6.is_loopback()
                        && !ip6.is_unspecified()
                        && (ip6.segments()[0] & 0xffc0) != 0xfe80
                    {
                        let p2p_addr = address.clone().with(Protocol::P2p(*swarm.local_peer_id()));
                        swarm.add_external_address(p2p_addr.clone());
                        tracing::info!("👉  Added {p2p_addr}");
                        break p2p_addr;
                    }
                }
                Some(Protocol::Ip4(_ip4)) => {
                    let p2p_addr = address.clone().with(Protocol::P2p(*swarm.local_peer_id()));
                    swarm.add_external_address(p2p_addr.clone());
                    tracing::info!("👉  Added {p2p_addr}");
                    break p2p_addr;
                }
                _ => {
                    tracing::warn!("Unknown address type: {address}");
                }
            }
        }
    };

    tracing::info!(%address, "Listening on");

    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let server_handle = tokio::spawn(serve(address.clone(), shutdown_rx));

    let mut qid = None;

    // loop until we receive a request_response from the browser with the CID they
    // have that we want to ask for.
    loop {
        if let SwarmEvent::Behaviour(test_utils::BehaviourEvent::RequestResponse(
            libp2p::request_response::Event::Message {
                message:
                    Message::Request {
                        request: PeerRequest(cid_bytes),
                        ..
                    },
                ..
            },
        )) = swarm.select_next_some().await
        {
            tracing::info!(?cid_bytes, "Received request with CID");

            if let Ok(cid) = Cid::try_from(cid_bytes) {
                // request this cid vis bitswap
                tracing::info!(?cid, "Requesting CID via bitswap");
                qid.replace(swarm.behaviour_mut().bitswap.get(&cid));
                break;
            }
        }
    }

    // wait for the block to be fetched
    // we'll know because it's a beetswap::Event::GetQueryResponse { query_id, data }
    // and the data should be the same as TEST_BLOCK_DATA
    let block = loop {
        let event = swarm.select_next_some().await;
        if let SwarmEvent::Behaviour(test_utils::BehaviourEvent::Bitswap(
            beetswap::Event::GetQueryResponse { query_id, data },
        )) = event
        {
            tracing::info!(?query_id, "🎉 Received block data!");
            if let Some(qid) = qid {
                if qid == query_id {
                    tracing::info!("🎉🎉 Block data matches!");
                    break data;
                }
            }
        } else {
            tracing::warn!(?event, "Ignoring event {:?}", event);
        }
    };

    // check the block data
    assert_eq!(block, test_utils::TEST_BLOCK_DATA);

    tracing::info!("🎉🎉🎉Test passed!");

    // Send shutdown signal
    shutdown_tx.send(()).unwrap();

    // Await server handle to ensure it has shutdown
    let _ = server_handle.await.unwrap();

    Ok(())
}

#[derive(rust_embed::RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/static"]
struct StaticFiles;

/// Serve the Multiaddr we are listening on and the host files.
pub(crate) async fn serve(
    libp2p_transport: Multiaddr,
    mut shutdown: watch::Receiver<()>,
) -> Result<()> {
    let Some(Protocol::Ip4(_listen_addr)) = libp2p_transport.iter().next() else {
        panic!("Expected 1st protocol to be IP")
    };

    let server = Router::new()
        .route("/", get(get_index))
        .route("/index.html", get(get_index))
        .route("/:path", get(get_static_file))
        // Report tests status
        .with_state(Libp2pState {
            endpoint: libp2p_transport.clone(),
        })
        .layer(
            // allow cors
            CorsLayer::new()
                .allow_origin(cors::Any)
                .allow_methods([http::Method::GET]),
        );

    let serve_addr_ipv4 = Ipv4Addr::new(127, 0, 0, 1);

    let addr = SocketAddr::new(serve_addr_ipv4.into(), 8080);

    tracing::info!(url=%format!("http://{addr}"), "Serving client files at url");

    let mut shut = shutdown.clone();

    tokio::spawn(async move {
        let server = axum::Server::bind(&addr).serve(server.into_make_service());
        let graceful = server.with_graceful_shutdown(async {
            shut.changed().await.ok();
        });
        graceful.await.ok();
    });

    let (mut chrome, driver) = open_in_browser(&format!("http://{addr:?}"))
        .await
        .map_err(|e| tracing::error!(?e, "Failed to open browser"))
        .unwrap();

    // shutdown or 15 second
    match tokio::time::timeout(Duration::from_secs(15), shutdown.changed()).await {
        Ok(_) => {
            tracing::info!("Received shutdown signal");
        }
        Err(_) => {
            tracing::info!("Timeout reached, closing browser");
        }
    }

    tracing::info!("Closing browser");

    // Close the browser after we got the results
    driver.quit().await?;
    chrome.kill().await?;

    tracing::info!("Browser closed");

    Ok(())
}

#[derive(Clone)]
struct Libp2pState {
    endpoint: Multiaddr,
}

/// Serves the index.html file for our client.
///
/// Our server listens on a random UDP port for the WebRTC transport.
/// To allow the client to connect, we replace the `__LIBP2P_ENDPOINT__` placeholder with the actual address.
async fn get_index(
    State(Libp2pState {
        endpoint: libp2p_endpoint,
        ..
    }): State<Libp2pState>,
) -> Result<Html<String>, StatusCode> {
    let content = StaticFiles::get("index.html")
        .ok_or(StatusCode::NOT_FOUND)?
        .data;

    let html = std::str::from_utf8(&content)
        .expect("index.html to be valid utf8")
        .replace("__LIBP2P_ENDPOINT__", &libp2p_endpoint.to_string());

    Ok(Html(html))
}

/// Serves the static files generated by `wasm-pack`.
async fn get_static_file(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    tracing::debug!(file_path=%path, "Serving static file");

    let content = StaticFiles::get(&path).ok_or(StatusCode::NOT_FOUND)?.data;
    let content_type = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();

    Ok(([(CONTENT_TYPE, content_type)], content))
}

async fn open_in_browser(addr: &str) -> Result<(Child, WebDriver)> {
    // start a webdriver process
    tracing::info!("Starting chromedriver");
    // currently only the chromedriver is supported as firefox doesn't
    // have support yet for the certhashes
    let chromedriver = if cfg!(windows) {
        "chromedriver.cmd"
    } else {
        "chromedriver"
    };
    let mut chrome = tokio::process::Command::new(chromedriver)
        .arg("--port=45782")
        .stdout(Stdio::piped())
        .spawn()?;
    // read driver's stdout
    let driver_out = chrome
        .stdout
        .take()
        .context("No stdout found for webdriver")?;
    // wait for the 'ready' message
    let mut reader = BufReader::new(driver_out).lines();
    while let Some(line) = reader.next_line().await? {
        tracing::debug!(?line);
        if line.contains("ChromeDriver was started successfully") {
            break;
        }
    }

    tracing::info!("Launching headless chrome");

    // run a webdriver client
    let mut caps = DesiredCapabilities::chrome();
    caps.set_headless()
        .map_err(|_| anyhow::anyhow!("Failed to set headless"))?;
    caps.set_disable_dev_shm_usage()
        .map_err(|_| anyhow::anyhow!("Failed to set disable_dev_shm_usage"))?;
    caps.set_no_sandbox()
        .map_err(|_| anyhow::anyhow!("Failed to set no_sandbox"))?;

    let driver = WebDriver::new("http://localhost:45782", caps)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create web driver: {:?}", e))?;

    driver
        .goto("http://127.0.0.1:8080/")
        .await
        .map_err(|e| anyhow::anyhow!("Failed to navigate to test service: {:?}", e))?;

    tracing::info!("♥️  Web is ready");

    Ok((chrome, driver))
}
