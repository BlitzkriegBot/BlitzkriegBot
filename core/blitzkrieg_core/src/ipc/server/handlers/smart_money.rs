use super::prelude::*;
use crate::schema::{ErrorData, Failure, method};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyzeParams {
    wallet: String,
    #[serde(default)]
    market: Option<String>,
}

pub(crate) async fn handle_smart_money(
    method: &str,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    let fetcher = blitzkrieg_onchain_pull::HttpFetcher::new(120);
    match method {
        method::SMART_MONEY_SCAN => {
            let req: blitzkrieg_onchain_pull::radar::RadarScanRequest =
                serde_json::from_value(params).map_err(|e| {
                    (
                        Failure::INVALID_PARAMS,
                        format!("invalid smart_money.scan params: {e}"),
                        None,
                    )
                })?;
            let res = blitzkrieg_onchain_pull::radar::scan_leaderboard(&fetcher, &req)
                .await
                .map_err(|e| (Failure::APPLICATION, e, None))?;
            serde_json::to_value(res).map_err(|e| (Failure::APPLICATION, e.to_string(), None))
        }
        method::SMART_MONEY_ANALYZE => {
            let req: AnalyzeParams = serde_json::from_value(params).map_err(|e| {
                (
                    Failure::INVALID_PARAMS,
                    format!("invalid smart_money.analyze params: {e}"),
                    None,
                )
            })?;
            let res = blitzkrieg_onchain_pull::radar::analyze_wallet_trades(
                &fetcher,
                &req.wallet,
                req.market.as_deref(),
            )
            .await
            .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
            serde_json::to_value(res).map_err(|e| (Failure::APPLICATION, e.to_string(), None))
        }
        _ => unreachable!("family handler called with a verb of another family"),
    }
}
