use axum::{routing::post, Router};
use clap::Parser;
use dotenvy::dotenv;
use events::handle_message_received;
use karak_rs::p2p::GossipMessage;
use libp2p::Multiaddr;
use metrics::describe_counter;
use metrics_exporter_prometheus::PrometheusBuilder;
use p2p::{p2p_init, parse_bootstrap_nodes};
use rusqlite::Connection;
use server::{query_payloads, AppState};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use wormhole_operator::{
    constants::TOPIC,
    events::run_event_listener,
    register::register_operator,
    utils::{is_operator_registered_in_listening_chains, load_config},
    WormholeOperator, WormholeOperatorCommand,
};

pub use wormhole_operator::{constants, contracts, events, keypair, p2p, server, table};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv().ok();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing::info!("Starting wormhole-dss-operator");

    let cli = WormholeOperator::parse();

    match cli.command {
        WormholeOperatorCommand::Run { .. } => {
            let config = Arc::new(load_config(cli).await?);

            //tracing subscriber
            let filter_layer = EnvFilter::from_default_env();
            let fmt_layer = fmt::layer().compact();
            let subscriber = tracing_subscriber::registry().with(filter_layer).with(fmt_layer);

            subscriber.init();

            PrometheusBuilder::new().install()?;
            describe_counter!("event_logs_read", "Counts the number of event logs read");
            describe_counter!(
                "messages_received_from_p2p",
                "Counts the number of messages received from p2p network"
            );
            describe_counter!(
                "messages_sent_to_p2p",
                "Counts the number of messages sent to p2p network"
            );
            describe_counter!(
                "failed_to_send_messages",
                "Counts the number of messages failed to send to p2p network"
            );
            describe_counter!("payloads_processed", "Counts the number of payloads processed");
            describe_counter!("api_requests", "Counts the number of api requests");

            if !is_operator_registered_in_listening_chains(&config).await? {
                panic!("Operator is not registered in all listening chains, please register by running the wormhole-dss-operator register command first");
            }
            let connection = Arc::new(Mutex::new(Connection::open(&config.env_config.db_path)?));

            let connection_p2p = connection.clone();
            let connection_server = connection.clone();
            let connection_event_listener = connection.clone();

            let config_p2p = config.clone();
            let config_server = config.clone();
            let config_event_listener = config.clone();

            // p2p
            let (termination_signal, termination_receiver) = oneshot::channel::<()>();
            let (message_sender, message_receiver) = mpsc::channel::<GossipMessage<String>>(100);
            let p2p_handle: JoinHandle<eyre::Result<()>> = tokio::spawn(async move {
                let bootstrap_nodes = parse_bootstrap_nodes(&config.env_config.bootstrap_nodes)?;
                p2p_init(
                    TOPIC,
                    config.env_config.p2p_listen_address.parse::<Multiaddr>()?,
                    bootstrap_nodes,
                    termination_receiver,
                    message_receiver,
                    config.env_config.idle_timeout_duration,
                    move |_peer_id, _message_id, message| {
                        let callback_connection = connection_p2p.clone();
                        let callback_config = config_p2p.clone();
                        async move {
                            handle_message_received(message, callback_connection, callback_config)
                                .await
                                .unwrap_or_else(|e| panic!("Handle message received failed {}", e));
                        }
                    },
                )
                .await?;
                Ok(())
            });

            // axum server

            let addr = SocketAddr::from(([0, 0, 0, 0], config_server.env_config.server_port));
            let listener = tokio::net::TcpListener::bind(addr).await?;
            let state = Arc::new(AppState { db: connection_server, config: config_server });
            let app: Router =
                Router::new().route("/query_payloads", post(query_payloads)).with_state(state);
            let (server_termination_signal, server_termination_receiver) = oneshot::channel::<()>();
            let server_handle: JoinHandle<eyre::Result<()>> = tokio::spawn(async move {
                tracing::info!("Server is listening on {}", addr);
                axum::serve(listener, app.into_make_service())
                    .with_graceful_shutdown(async {
                        server_termination_receiver
                            .await
                            .expect("Failed to receive termination signal");
                        tracing::info!("Shutting down server")
                    })
                    .await?;

                Ok(())
            });

            let event_listener_handle: JoinHandle<eyre::Result<()>> = tokio::spawn(async move {
                run_event_listener(
                    config_event_listener,
                    &connection_event_listener,
                    &message_sender,
                    TOPIC,
                )
                .await?;

                Ok(())
            });

            let graceful_shutdown_handle: JoinHandle<eyre::Result<()>> = tokio::spawn(async move {
                tokio::signal::ctrl_c().await.expect("Failed to install CTRL+C handler");
                tracing::info!("Received termination signal");
                termination_signal.send(()).expect("Failed to send termination signal");
                server_termination_signal
                    .send(())
                    .expect("Failed to send server termination signal");
                Ok(())
            });

            let _ = server_handle.await?;
            let _ = p2p_handle.await?;
            let _ = event_listener_handle.await?;
            let _ = graceful_shutdown_handle.await?;
        }
        WormholeOperatorCommand::Register => {
            //tracing subscriber
            let filter_layer = EnvFilter::from_default_env();
            let fmt_layer = fmt::layer().compact();
            let subscriber = tracing_subscriber::registry().with(filter_layer).with(fmt_layer);

            tracing::subscriber::set_global_default(subscriber).expect("Failed to set subscriber");

            register_operator(cli).await?;
        }
    }
    Ok(())
}
