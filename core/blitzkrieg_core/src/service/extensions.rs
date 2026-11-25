//! Extension + feed subscription methods on `Core`. Pure code motion from
//! `service.rs` (no logic edits).

use crate::service::Core;
use crate::service::CoreExtensionContext;

impl Core {
    /// Enable an extension (runs `on_load`). The extension receives ONLY a
    /// narrowed context — no credentials, venue client, socket or internal state.
    pub async fn enable_extension(&mut self, name: &str) -> Result<(), String> {
        let ctx = CoreExtensionContext {
            tx: self.tx.clone(),
            strategies: self.strategy_names(),
        };
        self.extensions.enable(name, &ctx).await
    }

    /// Disable an extension (runs `on_unload`).
    pub async fn disable_extension(&mut self, name: &str) -> Result<(), String> {
        self.extensions.disable(name).await
    }

    /// Subscribe the round's tokens on the NAMED plugin's orderbook feed (P4).
    /// Only that plugin's control sees the set — replacement semantics stay
    /// per-venue. Unknown name or empty token list: no-op.
    ///
    /// SYNCHRONOUS on purpose (#182): the body is a `try_send` onto the feed's
    /// control channel — there is no suspend point in it, and every caller
    /// runs it while HOLDING the core guard (`handlers/engine.rs`,
    /// `handlers/round.rs`, `market/host.rs::subscribe_tokens`). As an `async
    /// fn` it put a `.await` inside those guard scopes, so the day the callee
    /// grew real I/O every account would hang behind one venue's subscription
    /// write. A sync `fn` cannot acquire that hazard.
    pub fn subscribe_feed_tokens(&self, plugin: &str, tokens: Vec<String>) {
        // An empty set is a real unsubscribe (the last account left this
        // family's high-freq subscription). Skipping empty would leave the
        // previous tokens polling forever.
        if let Some(feed) = self.feeds.get(plugin) {
            feed.set_tokens(tokens);
        }
    }

    /// See [`Core::subscribe_feed_tokens`] — same guard scope, same reason.
    pub fn subscribe_placeholder_tokens(&self, plugin: &str, tokens: Vec<String>) {
        if let Some(feed) = self.feeds.get(plugin) {
            feed.set_placeholder_tokens(tokens);
        }
    }

    /// Push the plane's live / placeholder token sets to the named plugin.
    ///
    /// SYNCHRONOUS on purpose (#182): both halves are the `try_send` controls
    /// above. Callers hold the core guard (see `subscribe_feed_tokens`).
    pub fn publish_family_feed_subscriptions(&self, plugin: &str) {
        let (live, placeholder) = {
            let plane = self.family_feed();
            (
                plane.live_tokens_for(plugin),
                plane.placeholder_tokens_for(plugin),
            )
        };
        self.subscribe_feed_tokens(plugin, live);
        self.subscribe_placeholder_tokens(plugin, placeholder);
    }
}
