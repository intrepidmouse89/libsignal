//
// Copyright 2023 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::panic::RefUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libsignal_net::connect_state::{
    ConnectState, DefaultConnectorFactory, PreconnectingFactory, SUGGESTED_CONNECT_CONFIG,
    SUGGESTED_TLS_PRECONNECT_LIFETIME,
};
use libsignal_net::enclave::{Cdsi, EnclaveEndpoint, EnclaveEndpointConnection, EnclaveKind};
use libsignal_net::env::{add_user_agent_header, Env, UserAgent};
use libsignal_net::infra::connection_manager::MultiRouteConnectionManager;
use libsignal_net::infra::dns::DnsResolver;
use libsignal_net::infra::route::ConnectionProxyConfig;
use libsignal_net::infra::tcp_ssl::{InvalidProxyConfig, TcpSslConnector};
use libsignal_net::infra::timeouts::ONE_ROUTE_CONNECTION_TIMEOUT;
use libsignal_net::infra::utils::ObservableEvent;
use libsignal_net::infra::{EnableDomainFronting, EndpointConnection};

use crate::*;

pub mod cdsi;
pub mod chat;
pub mod tokio;

pub use tokio::TokioAsyncContext;

#[derive(num_enum::TryFromPrimitive)]
#[repr(u8)]
#[derive(Clone, Copy, strum::Display)]
pub enum Environment {
    Staging = 0,
    Prod = 1,
}

impl Environment {
    pub fn env(self) -> Env<'static> {
        match self {
            Self::Staging => libsignal_net::env::STAGING,
            Self::Prod => libsignal_net::env::PROD,
        }
    }
}

struct EndpointConnections {
    chat: EndpointConnection<MultiRouteConnectionManager>,
    cdsi: EnclaveEndpointConnection<Cdsi, MultiRouteConnectionManager>,
    enable_fronting: EnableDomainFronting,
}

impl EndpointConnections {
    fn new(
        env: &Env<'static>,
        user_agent: &UserAgent,
        use_fallbacks: bool,
        network_change_event: &ObservableEvent,
    ) -> Self {
        log::info!(
            "Creating endpoint connections (fallbacks {}) for {} and others",
            if use_fallbacks { "enabled" } else { "disabled" },
            // Note: this is *not* using log_safe_domain, because it is always the direct route.
            // Either it's chat.signal.org, chat.staging.signal.org, or something that indicates
            // testing. (Or the person running this isn't Signal.)
            env.chat_domain_config.connect.hostname
        );
        let chat = libsignal_net::chat::endpoint_connection(
            &env.chat_domain_config.connect,
            user_agent,
            use_fallbacks,
            network_change_event,
        );
        let cdsi =
            Self::endpoint_connection(&env.cdsi, user_agent, use_fallbacks, network_change_event);
        Self {
            chat,
            cdsi,
            enable_fronting: if use_fallbacks {
                EnableDomainFronting::OneDomainPerProxy
            } else {
                EnableDomainFronting::No
            },
        }
    }

    fn endpoint_connection<E: EnclaveKind>(
        endpoint: &EnclaveEndpoint<'static, E>,
        user_agent: &UserAgent,
        include_fallback: bool,
        network_change_event: &ObservableEvent,
    ) -> EnclaveEndpointConnection<E, MultiRouteConnectionManager> {
        let params = if include_fallback {
            endpoint
                .domain_config
                .connect
                .connection_params_with_fallback()
        } else {
            vec![endpoint.domain_config.connect.direct_connection_params()]
        };
        let params = add_user_agent_header(params, user_agent);
        EnclaveEndpointConnection::new_multi(
            endpoint,
            params,
            ONE_ROUTE_CONNECTION_TIMEOUT,
            network_change_event,
        )
    }
}

pub struct ConnectionManager {
    env: Env<'static>,
    user_agent: UserAgent,
    dns_resolver: DnsResolver,
    connect: ::tokio::sync::RwLock<ConnectState<PreconnectingFactory>>,
    // We could split this up to a separate mutex on each kind of connection,
    // but we don't hold it for very long anyway (just enough to clone the Arc).
    endpoints: std::sync::Mutex<Arc<EndpointConnections>>,
    transport_connector: std::sync::Mutex<TcpSslConnector>,
    most_recent_network_change: std::sync::Mutex<Instant>,
    network_change_event: ObservableEvent,
}

impl RefUnwindSafe for ConnectionManager {}

impl ConnectionManager {
    pub fn new(environment: Environment, user_agent: &str) -> Self {
        log::info!("Initializing connection manager for {}...", &environment);
        Self::new_from_static_environment(environment.env(), user_agent)
    }

    pub fn new_from_static_environment(env: Env<'static>, user_agent: &str) -> Self {
        let network_change_event = ObservableEvent::new();
        let user_agent = UserAgent::with_libsignal_version(user_agent);

        let dns_resolver =
            DnsResolver::new_with_static_fallback(env.static_fallback(), &network_change_event);
        let transport_connector =
            std::sync::Mutex::new(TcpSslConnector::new_direct(dns_resolver.clone()));
        let endpoints = std::sync::Mutex::new(
            EndpointConnections::new(&env, &user_agent, false, &network_change_event).into(),
        );
        Self {
            env,
            endpoints,
            user_agent,
            connect: ConnectState::new_with_transport_connector(
                SUGGESTED_CONNECT_CONFIG,
                PreconnectingFactory::new(
                    DefaultConnectorFactory,
                    SUGGESTED_TLS_PRECONNECT_LIFETIME,
                ),
            ),
            dns_resolver,
            transport_connector,
            most_recent_network_change: Instant::now().into(),
            network_change_event,
        }
    }

    pub fn set_proxy(&self, proxy: ConnectionProxyConfig) {
        let mut guard = self.transport_connector.lock().expect("not poisoned");
        guard.set_proxy(proxy);
    }

    pub fn set_invalid_proxy(&self) {
        let mut guard = self.transport_connector.lock().expect("not poisoned");
        guard.set_invalid();
    }

    pub fn clear_proxy(&self) {
        let mut guard = self.transport_connector.lock().expect("not poisoned");
        guard.clear_proxy();
    }

    pub fn is_using_proxy(&self) -> Result<bool, InvalidProxyConfig> {
        let guard = self.transport_connector.lock().expect("not poisoned");
        guard.proxy().map(|proxy| proxy.is_some())
    }

    pub fn set_ipv6_enabled(&self, ipv6_enabled: bool) {
        let mut guard = self.transport_connector.lock().expect("not poisoned");
        guard.set_ipv6_enabled(ipv6_enabled);
        self.connect.blocking_write().route_resolver.allow_ipv6 = ipv6_enabled;
    }

    /// Resets the endpoint connections to include or exclude censorship circumvention routes.
    ///
    /// This is not itself a network change event; existing working connections are expected to
    /// continue to work, and existing failing connections will continue to fail.
    pub fn set_censorship_circumvention_enabled(&self, enabled: bool) {
        let new_endpoints = EndpointConnections::new(
            &self.env,
            &self.user_agent,
            enabled,
            &self.network_change_event,
        );
        *self.endpoints.lock().expect("not poisoned") = Arc::new(new_endpoints);
    }

    const NETWORK_CHANGE_DEBOUNCE: Duration = Duration::from_secs(1);

    pub fn on_network_change(&self, now: Instant) {
        {
            let mut most_recent_change_guard = self
                .most_recent_network_change
                .lock()
                .expect("not poisoned");
            if now.saturating_duration_since(*most_recent_change_guard)
                < Self::NETWORK_CHANGE_DEBOUNCE
            {
                log::info!("ConnectionManager: on_network_change (debounced)");
                return;
            }
            *most_recent_change_guard = now;
        }
        log::info!("ConnectionManager: on_network_change");
        self.network_change_event.fire();
        self.connect.blocking_write().network_changed(now.into());
    }
}

bridge_as_handle!(ConnectionManager);
bridge_as_handle!(ConnectionProxyConfig);

#[cfg(test)]
mod test {
    use ::tokio; // otherwise ambiguous with the tokio submodule
    use assert_matches::assert_matches;
    use libsignal_net::chat::ConnectError;
    use test_case::test_case;

    use super::*;
    use crate::net::chat::UnauthenticatedChatConnection;

    #[test_case(Environment::Staging; "staging")]
    #[test_case(Environment::Prod; "prod")]
    fn can_create_connection_manager(env: Environment) {
        let _ = ConnectionManager::new(env, "test-user-agent");
    }

    // Normally we would write this test in the app languages, but it depends on timeouts.
    // Using a paused tokio runtime auto-advances time when there's no other work to be done.
    #[tokio::test(start_paused = true)]
    async fn cannot_connect_through_invalid_proxy() {
        let cm = ConnectionManager::new(Environment::Staging, "test-user-agent");
        cm.set_invalid_proxy();
        let err = UnauthenticatedChatConnection::connect(&cm)
            .await
            .map(|_| ())
            .expect_err("should fail to connect");
        assert_matches!(err, ConnectError::InvalidConnectionConfiguration);
    }

    #[test]
    fn network_change_event_debounced() {
        let cm = ConnectionManager::new(Environment::Staging, "test-user-agent");

        let fire_count = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let _subscription = {
            let fire_count = fire_count.clone();
            cm.network_change_event.subscribe(Box::new(move || {
                _ = fire_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }))
        };

        // The creation of the ConnectionManager sets the initial debounce timestamp,
        // so let's say our first even happens well after that.
        let start = Instant::now() + ConnectionManager::NETWORK_CHANGE_DEBOUNCE * 10;
        cm.on_network_change(start);
        assert_eq!(1, fire_count.load(std::sync::atomic::Ordering::SeqCst));

        cm.on_network_change(start);
        assert_eq!(1, fire_count.load(std::sync::atomic::Ordering::SeqCst));

        cm.on_network_change(start + ConnectionManager::NETWORK_CHANGE_DEBOUNCE / 2);
        assert_eq!(1, fire_count.load(std::sync::atomic::Ordering::SeqCst));

        cm.on_network_change(start + ConnectionManager::NETWORK_CHANGE_DEBOUNCE);
        assert_eq!(2, fire_count.load(std::sync::atomic::Ordering::SeqCst));

        cm.on_network_change(start);
        assert_eq!(2, fire_count.load(std::sync::atomic::Ordering::SeqCst));

        cm.on_network_change(start + ConnectionManager::NETWORK_CHANGE_DEBOUNCE * 3 / 2);
        assert_eq!(2, fire_count.load(std::sync::atomic::Ordering::SeqCst));

        cm.on_network_change(start + ConnectionManager::NETWORK_CHANGE_DEBOUNCE * 4);
        assert_eq!(3, fire_count.load(std::sync::atomic::Ordering::SeqCst));
    }
}
