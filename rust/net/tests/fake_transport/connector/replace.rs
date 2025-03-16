//
// Copyright 2025 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use libsignal_net_infra::route::{ComposedConnector, DirectOrProxy, ThrottlingConnector};

use super::FakeTransportConnector;

/// Replaces `self`'s [`Connector`]s with [`FakeTransportConnector`].
pub trait ReplaceStatelessConnectorsWithFake {
    /// The type after replacement.
    type Replacement;

    /// Consumes `self` and swaps out all "real" `Connector`s.
    fn replace_with_fake(self, fake: FakeTransportConnector) -> Self::Replacement;
}

impl<Outer, Inner, Error> ReplaceStatelessConnectorsWithFake
    for ComposedConnector<Outer, Inner, Error>
where
    Outer: ReplaceStatelessConnectorsWithFake,
    Inner: ReplaceStatelessConnectorsWithFake,
{
    type Replacement = ComposedConnector<Outer::Replacement, Inner::Replacement, Error>;

    fn replace_with_fake(self, fake: FakeTransportConnector) -> Self::Replacement {
        let (outer, inner) = self.into_connectors();
        ComposedConnector::new(
            outer.replace_with_fake(fake.clone()),
            inner.replace_with_fake(fake),
        )
    }
}

impl<D, P, E> ReplaceStatelessConnectorsWithFake for DirectOrProxy<D, P, E>
where
    D: ReplaceStatelessConnectorsWithFake,
    P: ReplaceStatelessConnectorsWithFake,
{
    type Replacement = DirectOrProxy<D::Replacement, P::Replacement, E>;

    fn replace_with_fake(self, fake: FakeTransportConnector) -> Self::Replacement {
        let (direct, proxy) = self.into_connectors();
        DirectOrProxy::new(
            direct.replace_with_fake(fake.clone()),
            proxy.replace_with_fake(fake),
        )
    }
}

impl ReplaceStatelessConnectorsWithFake for libsignal_net::infra::tcp_ssl::proxy::StatelessProxied {
    type Replacement = FakeTransportConnector;

    fn replace_with_fake(self, fake: FakeTransportConnector) -> Self::Replacement {
        fake
    }
}

impl ReplaceStatelessConnectorsWithFake for libsignal_net::infra::tcp_ssl::StatelessDirect {
    type Replacement = FakeTransportConnector;

    fn replace_with_fake(self, fake: FakeTransportConnector) -> Self::Replacement {
        fake
    }
}

impl<C: ReplaceStatelessConnectorsWithFake> ReplaceStatelessConnectorsWithFake
    for ThrottlingConnector<C>
{
    type Replacement = ThrottlingConnector<FakeTransportConnector>;

    fn replace_with_fake(self, fake: FakeTransportConnector) -> Self::Replacement {
        self.replace_connector(fake)
    }
}
