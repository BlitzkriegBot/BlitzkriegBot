//! Built-in example extensions.
//!
//! These demonstrate the extension contract and keep `extension.list` non-empty
//! so lifecycle operations are exercisable without an external plugin.

use super::async_trait_lite::async_trait;
use super::{Extension, ExtensionContext, ExtensionType};

/// A market extension shell for Chainlink 60s TWAP. It proves the extension
/// point: it can be installed/enabled/disabled and logs on load, while the
/// actual oracle feed rides `chainlink_extension::ChainlinkPlugin`.
#[derive(Default)]
pub struct ChainlinkExtension;

impl ChainlinkExtension {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extension for ChainlinkExtension {
    fn name(&self) -> &str {
        "chainlink"
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn extension_type(&self) -> ExtensionType {
        ExtensionType::Market
    }

    async fn on_load(&self, ctx: &dyn ExtensionContext) -> Result<(), String> {
        ctx.log(&format!(
            "chainlink extension loaded (strategies: {:?})",
            ctx.strategy_names()
        ));
        Ok(())
    }

    async fn on_unload(&self) -> Result<(), String> {
        Ok(())
    }
}
