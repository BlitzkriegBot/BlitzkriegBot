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

    /// The ONLY write entry for one venue's feed subscription (#183).
    ///
    /// `set_tokens` is REPLACE semantics, so the only safe input is a COMPLETE
    /// union for the venue — and the `(venue, family)` plane is the one place
    /// that union lives. Every caller that can move a venue's subscription
    /// asks this method; a handler that computed its own token list is exactly
    /// how `engine.markets` used to evict sibling families.
    ///
    /// The three shapes are the ones `CoreHost::subscribe_tokens` has always
    /// routed — mirrored here so ONE semantic exists, not two:
    ///
    /// 1. **This venue has a configured account** → publish the plane's union
    ///    (live families + placeholder families).
    /// 2. **Some venue has an account, this one does not** → publish empty; an
    ///    unowned venue must not keep a connection.
    /// 3. **No accounts at all** (tests / account-less hosts) → pass the
    ///    caller's explicit token list through unchanged.
    ///
    /// `batch` is the truth for shape 3 only; shapes 1–2 derive everything
    /// from the plane and ignore it.
    ///
    /// SYNCHRONOUS on purpose (#182): the body is the same pair of `try_send`
    /// controls `publish_family_feed_subscriptions` is — there is no suspend
    /// point in it, and every caller runs it while HOLDING the core guard.
    /// Returning `()` rather than a `Result` is deliberate: the control
    /// channel's `set_tokens` has no failure channel, so a `Result` here could
    /// only ever be a fabricated `Ok` (R7.1 L4 forbids spelling "unknown" as
    /// success).
    pub fn republish_venue_subscription(&self, venue: &str, batch: Vec<String>) {
        let (configured, any_account) = {
            let plane = self.family_feed();
            (
                plane.venue_is_configured(venue),
                !plane.inspect().venues_with_accounts.is_empty(),
            )
        };
        if configured {
            self.publish_family_feed_subscriptions(venue);
        } else if any_account {
            self.subscribe_feed_tokens(venue, Vec::new());
            self.subscribe_placeholder_tokens(venue, Vec::new());
        } else {
            self.subscribe_feed_tokens(venue, batch);
        }
    }

    /// Write the RAW set onto the NAMED plugin's control.
    ///
    /// `pub(crate)` on purpose (#183): this is the primitive, not the entry.
    /// An IPC handler holding a hand-built token list is how `engine.markets`
    /// used to evict sibling families; out-of-crate callers must go through
    /// [`Core::republish_venue_subscription`].
    ///
    /// SYNCHRONOUS on purpose (#182): the body is a `try_send` onto the feed's
    /// control channel — there is no suspend point in it, and every caller
    /// runs it while HOLDING the core guard (`handlers/engine.rs`,
    /// `handlers/round.rs`, `market/host.rs::subscribe_tokens`). As an `async
    /// fn` it put a `.await` inside those guard scopes, so the day the callee
    /// grew real I/O every account would hang behind one venue's subscription
    /// write. A sync `fn` cannot acquire that hazard.
    pub(crate) fn subscribe_feed_tokens(&self, plugin: &str, tokens: Vec<String>) {
        // An empty set is a real unsubscribe (the last account left this
        // family's high-freq subscription). Skipping empty would leave the
        // previous tokens polling forever.
        if let Some(feed) = self.feeds.get(plugin) {
            feed.set_tokens(tokens);
        }
    }

    /// See [`Core::subscribe_feed_tokens`] — same visibility, same reason.
    pub(crate) fn subscribe_placeholder_tokens(&self, plugin: &str, tokens: Vec<String>) {
        if let Some(feed) = self.feeds.get(plugin) {
            feed.set_placeholder_tokens(tokens);
        }
    }

    /// Push the plane's live / placeholder token sets to the named plugin.
    ///
    /// `pub(crate)` on purpose (#183): this is the union half of
    /// [`Core::republish_venue_subscription`]; the choice between "publish the
    /// union" and "pass the batch through" is the decision that must keep one
    /// spelling, so it is not exposed as a second entry.
    ///
    /// SYNCHRONOUS on purpose (#182): both halves are the `try_send` controls
    /// above. Callers hold the core guard (see `subscribe_feed_tokens`).
    pub(crate) fn publish_family_feed_subscriptions(&self, plugin: &str) {
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
