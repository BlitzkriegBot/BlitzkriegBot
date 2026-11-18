//! Hot-parameter reads/writes (`strategy.params.get` / `strategy.params.set`).
//! Moved verbatim from `service/strategies.rs` (pure code motion).

use crate::model::{CoreError, CoreErrorCode};
use crate::schema::{
    STRATEGY_PARAMS_SET_NOTE, StrategyParamChange, StrategyParamsReadout, StrategyParamsSetUpdate,
    TunableView,
};
use rust_decimal::Decimal;
use strategy_logic::params::StrategyParams as KnobBag;

use super::Core;

impl Core {
    /// `strategy.params.get`: one strategy's declared tunables plus the value
    /// in force. The schema source is the strategy's OWN knob declaration
    /// (manifest tunables for Lua packages) — the kernel invents no domain,
    /// and a strategy that declared nothing reports an empty list.
    pub fn strategy_params_readout(&self, name: &str) -> StrategyParamsReadout {
        let specs: Vec<_> = self
            .engine
            .as_ref()
            .map(|e| {
                e.strategy_refs()
                    .iter()
                    .find(|s| s.name() == name)
                    .map(|s| s.evolvable_knobs())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let found = self.strategy_names().iter().any(|n| n == name);
        // The current value in force: the registry cell when one exists, else
        // the declared value (the same thing while no override ever landed).
        let mut tunables: Vec<TunableView> = specs
            .iter()
            .map(|k| {
                let value = self
                    .shadow_evolution
                    .params_for(name)
                    .and_then(|p| p.get(&k.name))
                    .unwrap_or(k.value);
                TunableView {
                    name: k.name.clone(),
                    // All in-tree declarations are decimal-typed; the `int`
                    // tag only exists on the Lua manifest surface, which the
                    // set path re-checks against the declared domain. Expose
                    // the knob names the kernel actually enforces.
                    r#type: None,
                    value: value.to_string(),
                    min: k.min.to_string(),
                    max: k.max.to_string(),
                }
            })
            .collect();
        tunables.sort_by(|a, b| a.name.cmp(&b.name));
        let evolvable = !specs.is_empty();
        let params_attached = self.has_hot_params();
        let note = if !found {
            Some(format!("strategy {name} is not loaded"))
        } else if !evolvable {
            Some("该策略没有可调参数（manifest 未声明 tunables 域）".to_string())
        } else {
            None
        };
        StrategyParamsReadout {
            name: name.to_string(),
            found,
            tunables,
            evolvable,
            params_attached,
            note,
        }
    }

    /// `strategy.params.set` (issue #481 G3): the tunables' operator write
    /// path — the `risk.setExit` posture (plan first, write later, audit the
    /// receipt). Validation is against the strategy's OWN declared domains;
    /// a refused patch moves nothing.
    ///
    /// 生效路径（本轮裁决）：写入 = `registry.publish`（该策略 cell 的新值）
    /// 加引擎侧 attach。进化关闭时 `rewire_hot_params` 会把覆盖层脱钩，所以
    /// 一次操作员写入要生效必须把覆盖层重新挂上。这与「关闭进化 = 行为与
    /// 改动前一致」的验收共存：脱钩语义是「没有写入时等于没有覆盖层」，而
    /// 一旦操作员显式写了 tunables，挂载覆盖层就是那个写入的直接后果 ——
    /// 下次 evaluate 起生效；重启后覆盖层消失，回落 manifest 默认（回执
    /// note 明说 `persisted: false`）。进化的梯度锁（±5%）刻意不用于此路径：
    /// 操作员编辑不是进化的一个变体步，只有声明域是硬边界。
    pub fn apply_strategy_params_set(
        &mut self,
        name: &str,
        knobs: &std::collections::BTreeMap<String, serde_json::Value>,
        actor: &str,
        reason: Option<String>,
        now_ms: i64,
    ) -> Result<crate::schema::StrategyParamsSetUpdate, CoreError> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        // The declaration lives on the live instance (E2-c); a strategy that
        // is not hosted has nothing to validate against and nothing to write.
        let specs = self
            .engine
            .as_ref()
            .and_then(|e| {
                e.strategy_refs()
                    .iter()
                    .find(|s| s.name() == name)
                    .map(|s| s.evolvable_knobs())
            })
            .unwrap_or_default();
        if specs.is_empty() {
            return Err(err(format!(
                "strategy {name} declares no tunables (nothing is writable); \
                 read the declared domains with strategy.params.get"
            )));
        }
        if knobs.is_empty() {
            return Err(err(
                "strategy.params.set needs at least one knob; read the declared \
                 domains with strategy.params.get"
                    .into(),
            ));
        }

        // Plan first: every knob must be declared, parse exactly (decimal
        // string or JSON number — no float path) and sit inside its domain.
        let mut plan: Vec<(String, Decimal)> = Vec::new();
        for (knob, raw) in knobs {
            let Some(spec) = specs.iter().find(|s| &s.name == knob) else {
                return Err(err(format!(
                    "knob {knob} is not declared by {name} — nothing was applied; \
                     the declared tunables are: {}",
                    specs
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            };
            let value = match raw {
                serde_json::Value::String(s) => Decimal::from_str_exact(s.trim()),
                serde_json::Value::Number(n) => Decimal::from_str_exact(&n.to_string()),
                other => {
                    return Err(err(format!(
                        "knob {knob}: expected a decimal string, got {other}"
                    )));
                }
            };
            let Ok(value) = value else {
                return Err(err(format!(
                    "knob {knob}: {raw} is not an exact decimal — nothing was applied"
                )));
            };
            if !spec.contains(value) {
                return Err(err(format!(
                    "knob {knob}: {value} is outside the declared domain [{}, {}] — \
                     nothing was applied",
                    spec.min, spec.max
                )));
            }
            plan.push((knob.clone(), value));
        }

        // Write phase: merge the patch over the cell's current values (a
        // single-knob patch keeps the strategy's other knobs in force), then
        // publish + attach so the strategy reads it on the next evaluate.
        let registry = self.shadow_evolution.registry();
        let mut next: KnobBag = registry
            .handle_for(name)
            .map(|c| (**c.load()).clone())
            .unwrap_or_else(|| KnobBag::from_knobs(&specs));
        let mut changes: Vec<StrategyParamChange> = Vec::new();
        for (knob, value) in &plan {
            let from = next.get(knob).unwrap_or(Decimal::ZERO).to_string();
            next.set(knob, *value);
            changes.push(StrategyParamChange {
                knob: knob.clone(),
                from,
                to: value.to_string(),
                effect: "live",
            });
        }
        registry.publish(name, next);
        // Attach the overlay to the engine (see the method doc for why this is
        // unconditional): the set path IS an overlay write.
        if let Some(e) = self.engine.as_mut() {
            e.set_hot_params(Some(std::sync::Arc::clone(&registry)));
        }

        for c in &changes {
            tracing::info!(
                target: "strategy_params",
                actor = %actor,
                strategy = %name,
                knob = c.knob,
                from = %c.from,
                to = %c.to,
                effect = c.effect,
                reason = reason.as_deref().unwrap_or(""),
                "strategy tunables update: {}.{} {} -> {}",
                name,
                c.knob,
                c.from,
                c.to,
            );
        }
        // The evolution audit log carries manual overrides; this write is one
        // (same shape `shadow_evolution_apply` records), so an operator edit
        // shows up next to every adopted promotion.
        let before = {
            let mut bag = KnobBag::from_knobs(&specs);
            // from-values as recorded in the change list (the pre-write cell
            // read raced nothing: both reads happened inside this lock).
            for c in &changes {
                if let Ok(v) = Decimal::from_str_exact(&c.from) {
                    bag.set(&c.knob, v);
                }
            }
            let mut m = crate::shadow_evolution::knobs::MutableParams::new();
            m.set_strategy(name, bag);
            m
        };
        let after = {
            let mut m = crate::shadow_evolution::knobs::MutableParams::new();
            if let Some(p) = registry.handle_for(name).map(|c| (**c.load()).clone()) {
                m.set_strategy(name, p);
            }
            m
        };
        self.shadow_evolution.audit_manual_override(
            name,
            now_ms,
            &before,
            &after,
            "operator tunables edit",
        );

        Ok(StrategyParamsSetUpdate {
            name: name.to_string(),
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason,
            persisted: false,
            note: STRATEGY_PARAMS_SET_NOTE,
        })
    }
}
