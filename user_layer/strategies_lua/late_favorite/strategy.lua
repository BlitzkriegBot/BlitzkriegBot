-- late_favorite — near-settlement favorite filtered by Digital Black-Scholes
-- fair probability edge, conservative multi-horizon realized spot volatility,
-- and short-horizon spot momentum confirmation. Hold to settlement; one entry
-- per condition per round; a cancelled or filled attempt is not re-entered.
--
-- maker_entry 0 (default): cross the ask as a taker (reason "late-taker").
-- maker_entry 1: rest a post-only bid at best_bid and cancel below min_time_left.
--
-- THESIS DEATH DECLARATION (#961)
--   entry thesis : the favorite priced inside [min_px, max_px] with positive
--                  Digital BS edge >= min_edge over ask + taker fee and aligned
--                  spot momentum is held to settlement for $1 redemption
--   death cond 1 : not applied — intra-round book variance is already priced
--   death cond 2 : not applied — settlement depends on expiry TWAP, not live book
--   death cond 3 : not applied — hold to settlement without stop-loss exits
--   enforcement  : bk_thesis_death returns false; the runtime injects no exit

function bk_thesis_death(_h)
    return false
end

-- BEGIN common/decimal.lua
-- common/decimal.lua — the ONE exact-decimal helper block for every Lua
-- strategy package (#439 candidate 2).
--
-- A decimal is `{ m = mantissa, s = scale }` meaning m / 10^s. Mantissas stay
-- far inside 64-bit integers for every value these strategies handle: prices
-- ≤ 1.0 with ≤ 10 decimals, and host-derived division results capped at 18
-- significant digits on parse (see dec_parse).
--
-- The block each package embeds is the text BETWEEN its
-- `-- BEGIN common/decimal.lua` and `-- END common/decimal.lua` markers —
-- byte-for-byte THIS file. The Lua sandbox has no require/dofile (poisoned in
-- user_layer/lua_runtime/src/sandbox.rs), so packages cannot share code at
-- runtime; the loader contract (§6.4: manifest.sha256 covers the whole entry)
-- means a loader-side injection would change a frozen contract. Embedding +
-- `scripts/lua-decimal-check.mjs` (byte-identity gate) is the honest middle:
-- one source of truth, ten verbatim copies, a gate that refuses divergence.
--
-- EDIT THIS FILE, then run `node scripts/lua-decimal-sync.mjs --apply` — it
-- re-embeds every package and re-signs each manifest's sha256. Never edit a
-- copy inside a package: the gate reds on the next CI run.
--
-- Provenance of the hardened variants (the 2026-09-30 rust_decimal 28-digit
-- mantissa incident, #94233fb9): the host's derived fields (mid/obi/
-- spread_pct are rust_decimal DIVISION results) can carry up to 28
-- significant digits. The pre-hardening copies either indexed POW10 past its
-- 18 entries (dec_cmp/dec_round2 → nil arithmetic → the adapter swallowed the
-- crashed evaluation as "no intents") or parsed the mantissa as a float.
-- dec_parse caps at 18 significant digits, dec_rescale truncates toward -inf
-- when the target scale is BELOW the value's, dec_cmp caps the common scale,
-- and dec_round2/dec_floor2 fall back to float exponentiation only past the
-- POW10 table. For every input with ≤ 18 significant digits — the entire
-- domain these strategies were written for — all functions are bit-identical
-- to the pre-hardening copies.

local POW10 = { [0] = 1 }
for i = 1, 18 do POW10[i] = POW10[i - 1] * 10 end

--- Parse a canonical decimal string ("0.4123", "-0.5", "0"). `nil` on
--- anything else — the caller's nil-guard mirrors the Rust `dec()?` chain.
local function dec_parse(str)
  if type(str) ~= "string" then return nil end
  local t = str:match("^%s*(.-)%s*$")
  if t == "" then return nil end
  local neg = false
  local c = t:sub(1, 1)
  if c == "-" then neg = true; t = t:sub(2)
  elseif c == "+" then t = t:sub(2) end
  local int_part, frac_part = t:match("^(%d*)%.?(%d*)$")
  if not int_part or (int_part == "" and frac_part == "") then return nil end
  -- Host decimals can carry up to 28 significant digits (rust_decimal
  -- division output — obi/spread_pct). Keep the mantissa inside i64 by
  -- capping significant digits at 18: drop frac digits beyond what the
  -- integer part leaves room for. The 1e-12-scale wobble this can add is
  -- orders of magnitude below every threshold compared here.
  local keep = 18 - #int_part
  if #frac_part > keep then frac_part = frac_part:sub(1, keep) end
  local m = tonumber(int_part .. frac_part)
  if m == nil then return nil end
  local s = #frac_part
  if neg and m ~= 0 then m = -m end
  return { m = m, s = s }
end

--- rust_decimal `Display`: scale preserved exactly (zero included).
local function dec_fmt(d)
  local m, s = d.m, d.s
  local neg = m < 0
  if neg then m = -m end
  local digits = tostring(m)
  if s > 0 then
    if #digits <= s then digits = string.rep("0", s - #digits + 1) .. digits end
    return (neg and "-" or "") .. digits:sub(1, #digits - s) .. "." .. digits:sub(#digits - s + 1)
  end
  return (neg and "-" or "") .. digits
end

--- The shared alignment cap of dec_add/dec_sub/dec_cmp. The fee curve is the
--- live path here: `fee_per_share` MULTIPLIES scales (rate.s + 2·2·p.s), so a
--- 4-decimal ask (the venues' real quote grid, e.g. 0.3450) against the
--- legacy rate's 3 puts the fee at scale 19 and an unclamped alignment would
--- rescale both operands past POW10[18] — the mantissa wraps NEGATIVE
--- (9.9e18 > i64 max) before `POW10[total.s]` is even indexed, and the
--- wrapped total would slip every `< max_pair_cost` test. Truncation past
--- the 18th significant digit costs at most 1e-18 USD — orders of magnitude
--- below every threshold compared here (the tightest sits at the 3rd
--- decimal, max_pair_cost = 0.995).
local DEC_MAX_SCALE = 18

local function dec_rescale(d, s)
  -- Total: the host's rust_decimal fields (obi, spread_pct, depth sums) are
  -- DIVISION results carrying up to 28 significant digits, so s can sit
  -- BELOW d.s. Truncate toward -inf in that direction (an 1e-18-scale wobble
  -- is orders of magnitude below every threshold compared here); the
  -- multiply path stays exact while the shift fits POW10.
  if s >= d.s then return d.m * POW10[s - d.s] end
  return d.m // POW10[d.s - s]
end

local function dec_add(a, b)
  local s = math.max(math.min(a.s, DEC_MAX_SCALE), math.min(b.s, DEC_MAX_SCALE))
  return { m = dec_rescale(a, s) + dec_rescale(b, s), s = s }
end

local function dec_sub(a, b)
  local s = math.max(math.min(a.s, DEC_MAX_SCALE), math.min(b.s, DEC_MAX_SCALE))
  return { m = dec_rescale(a, s) - dec_rescale(b, s), s = s }
end

local function dec_mul(a, b)
  return { m = a.m * b.m, s = a.s + b.s }
end

local function dec_cmp(a, b)
  -- Cap the common scale (DEC_MAX_SCALE above): aligning a 28-digit obi
  -- against a 2-digit threshold would want POW10[26], which does not exist.
  -- Truncation past the 18th significant digit cannot flip a comparison
  -- whose tightest threshold sits at the 2nd decimal.
  local s = math.max(math.min(a.s, DEC_MAX_SCALE), math.min(b.s, DEC_MAX_SCALE))
  local am, bm = dec_rescale(a, s), dec_rescale(b, s)
  if am < bm then return -1 elseif am > bm then return 1 else return 0 end
end

--- Floor to the venue's 0.01 share grid (sizes are positive; integer
--- division floors).
local function dec_floor2(d)
  if d.s <= 2 then return { m = d.m * POW10[2 - d.s], s = 2 } end
  local div = POW10[d.s - 2] or 10 ^ (d.s - 2)
  return { m = d.m // div, s = 2 }
end

--- rust_decimal `round_dp(2)` — MidpointNearestEven, the exact `round2`.
--- The kernel's derived fields (mid/obi are rust_decimal RATIOS) can carry
--- up to 28 significant decimals, whose mantissa overflows Lua integers and
--- parses as a float — coerce the tiny result mantissa back to an integer so
--- dec_fmt never prints a float ("93.0").
local function dec_round2(d)
  if d.s <= 2 then return { m = d.m * POW10[2 - d.s], s = 2 } end
  local div = POW10[d.s - 2] or 10 ^ (d.s - 2)
  local q = d.m // div
  local r = d.m - q * div
  local twice = r * 2
  if twice > div or (twice == div and q % 2 == 1) then q = q + 1 end
  q = math.tointeger(q) or q
  return { m = q, s = 2 }
end

local function dec_param(params, name, fallback)
  local raw = params[name]
  if type(raw) ~= "string" then return fallback end
  local d = dec_parse(raw)
  return d or fallback
end

local function int_param(params, name, fallback)
  local raw = params[name]
  if type(raw) ~= "string" then return fallback end
  local n = tonumber(raw)
  if n == nil or n ~= math.floor(n) then return fallback end
  return math.floor(n)
end

-- END common/decimal.lua

local quoted = {}
local slot_seen = nil
local slot_max_tl = {}
local spot_by_sec = {}
local latest_spot = {}
local boundary_opens = {}
local last_prune_sec = {}

local function num_param(params, name, fallback)
    local n = tonumber(params[name])
    if n == nil or n ~= n then
        return fallback
    end
    return n
end

local function key_of(slot, cond)
    return tostring(slot) .. "\0" .. (cond or "")
end

local function held_index()
    local map = {}
    local hs = bk.holdings and bk.holdings() or {}
    if type(hs) ~= "table" then
        return map
    end
    for i = 1, #hs do
        local h = hs[i]
        if type(h) == "table" and type(h.token_id) == "string" then
            map[h.token_id] = true
        end
    end
    return map
end

local function note_round_tl(round)
    if round == nil or round.slot == nil then
        return
    end
    if round.slot ~= slot_seen then
        quoted = {}
        slot_seen = round.slot
        slot_max_tl = {}
    end
    if type(round.time_left_sec) == "number" then
        local prev = slot_max_tl[round.slot] or 0
        if round.time_left_sec > prev then
            slot_max_tl[round.slot] = round.time_left_sec
        end
    end
end

local function infer_dur_sec(max_tl)
    if type(max_tl) ~= "number" then
        return nil
    end
    if max_tl > 3605 then
        return 86400
    elseif max_tl > 905 then
        return 3600
    elseif max_tl > 305 then
        return 900
    elseif max_tl >= 240 then
        return 300
    end
    return nil
end

local function norm_cdf(x)
    local sign = 1.0
    if x < 0.0 then
        sign = -1.0
    end
    local ax = math.abs(x) / math.sqrt(2.0)
    local t = 1.0 / (1.0 + 0.3275911 * ax)
    local y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * math.exp(-ax * ax)
    return 0.5 * (1.0 + sign * y)
end

local function spot_at_sec(by_sec, target_sec)
    if by_sec == nil then
        return nil
    end
    for d = 0, 5 do
        local v = by_sec[target_sec - d]
        if v ~= nil and v > 0 then
            return v
        end
    end
    return nil
end

local function realized_sigma(by_sec, latest_sec, lookback_sec, step_sec)
    local prev = nil
    local sq_sum = 0.0
    local n_diffs = 0
    for t = latest_sec - lookback_sec, latest_sec, step_sec do
        local c = spot_at_sec(by_sec, t)
        if c == nil or c <= 0 then
            return nil
        end
        if prev ~= nil then
            local lr = math.log(c / prev)
            sq_sum = sq_sum + lr * lr
            n_diffs = n_diffs + 1
        end
        prev = c
    end
    if n_diffs < 5 then
        return nil
    end
    return math.sqrt((sq_sum / n_diffs) / step_sec)
end

local function taker_fee(p)
    local rate = 0.07
    local exponent = 1
    local f = bk.fees and bk.fees() or nil
    if type(f) == "table" and f.rate ~= nil then
        local r = tonumber(f.rate)
        local e = tonumber(f.exponent)
        if r ~= nil and r >= 0 and r == r then
            rate = r
        end
        if e ~= nil and e >= 0 and e <= 3 and e == math.floor(e) then
            exponent = math.floor(e)
        end
    end
    local base = p * (1.0 - p)
    if base <= 0 then
        return 0.0
    end
    local acc = 1.0
    for _ = 1, exponent do
        acc = acc * base
    end
    return rate * acc
end

function bk_on_kline(k)
    if type(k) ~= "table" or k.interval ~= "sec1" or k.is_closed ~= true then
        return
    end
    if type(k.symbol) ~= "string" or type(k.open_time_ms) ~= "number" or type(k.close_time_ms) ~= "number" then
        return
    end
    local close_px = tonumber(k.close)
    local open_px = tonumber(k.open)
    if close_px == nil or close_px <= 0 or close_px ~= close_px then
        return
    end
    if open_px == nil or open_px <= 0 or open_px ~= open_px then
        open_px = close_px
    end
    local sym = string.upper(k.symbol)
    local close_sec = math.floor(k.close_time_ms / 1000)
    local open_sec = math.floor(k.open_time_ms / 1000)

    local by_sec = spot_by_sec[sym]
    if by_sec == nil then
        by_sec = {}
        spot_by_sec[sym] = by_sec
    end
    by_sec[close_sec] = close_px

    local prev_latest = latest_spot[sym]
    if prev_latest == nil or k.close_time_ms >= prev_latest.close_ms then
        latest_spot[sym] = {
            close_ms = k.close_time_ms,
            close_sec = close_sec,
            price = close_px,
        }
    end

    local bmap = boundary_opens[sym]
    if bmap == nil then
        bmap = {}
        boundary_opens[sym] = bmap
    end
    local rem = open_sec % 300
    if rem >= 0 and rem <= 5 then
        local bucket = open_sec - rem
        local cur = bmap[bucket]
        if cur == nil or rem < cur.rem then
            bmap[bucket] = { rem = rem, price = open_px }
        end
    end

    local last_p = last_prune_sec[sym] or 0
    if close_sec - last_p >= 60 then
        last_prune_sec[sym] = close_sec
        for s, _ in pairs(by_sec) do
            if s < close_sec - 640 then
                by_sec[s] = nil
            end
        end
        for b, _ in pairs(bmap) do
            if b < close_sec - 90000 then
                bmap[b] = nil
            end
        end
    end
end

function bk_on_round(round)
    note_round_tl(round)
end

function bk_evaluate()
    local out = { entries = {}, exits = {}, breaks = {} }
    local round = bk.round()
    if round == nil or round.time_left_sec == nil or round.slot == nil then
        return out
    end
    note_round_tl(round)

    local params = bk.params()
    local maker = int_param(params, "maker_entry", 0) == 1
    local btc_only = int_param(params, "btc_only", 1) == 1
    local lo = num_param(params, "min_px", 0.75)
    local hi = num_param(params, "max_px", 0.86)
    local t_min = int_param(params, "min_time_left", 20)
    local t_max = int_param(params, "max_time_left", 120)
    local min_edge = num_param(params, "min_edge", 0.10)
    local mom_sec = int_param(params, "mom_window_sec", 15)
    local max_spot_age_ms = int_param(params, "max_spot_age_ms", 5000)
    local max_round_sec = int_param(params, "max_round_sec", 300)

    local tl = round.time_left_sec
    local dur_sec = infer_dur_sec(slot_max_tl[round.slot])
    local held = held_index()

    for _, market in ipairs(bk.markets()) do
        local cond = market.condition_id
        local sym = string.upper(market.asset or "")
        if cond ~= nil and (not btc_only or sym == "BTC") then
            local k = key_of(round.slot, cond)
            local st = quoted[k]
            local up_held = market.up_token ~= nil and held[market.up_token]
            local dn_held = market.down_token ~= nil and held[market.down_token]
            if st ~= nil and st.token ~= nil and held[st.token] then
                st.filled = true
            end
            if tl < t_min or tl > t_max then
                if maker and st ~= nil and st.token ~= nil and not st.filled and not st.cancelled then
                    out.breaks[#out.breaks + 1] = {
                        token = st.token,
                        broken_price = st.price,
                    }
                    st.cancelled = true
                end
            elseif st == nil and not up_held and not dn_held and dur_sec ~= nil and (max_round_sec <= 0 or dur_sec <= max_round_sec) then
                local spot = latest_spot[sym]
                local by_sec = spot_by_sec[sym]
                local bmap = boundary_opens[sym]
                local end_sec = math.floor((market.expires_at_ms or 0) / 1000)
                local open_rec = bmap and bmap[end_sec - dur_sec] or nil
                local spot_open = open_rec and open_rec.price or nil

                -- Freshness on the whole-second round clock only: end_sec - time_left_sec
                -- equals the unix second the kernel already exposes (time_left_sec is
                -- floor((expires - now) / 1000)). The host can then skip quiet
                -- millisecond ticks. 5 000 ms → 5 s, the same cutoff on every
                -- second boundary the event-driven path evaluates.
                local now_sec = end_sec - tl
                local max_age_sec = math.floor(max_spot_age_ms / 1000)
                if spot ~= nil and by_sec ~= nil and spot_open ~= nil and spot_open > 0
                    and (now_sec - (spot.close_sec or 0)) <= max_age_sec then
                    local spot_now = spot.price
                    local sig120 = realized_sigma(by_sec, spot.close_sec, 120, 10)
                    local sig300 = realized_sigma(by_sec, spot.close_sec, 300, 10)
                    local sig600 = realized_sigma(by_sec, spot.close_sec, 600, 10)
                    if sig120 ~= nil and sig300 ~= nil and sig600 ~= nil then
                        local sig = math.max(sig120, sig300, sig600)
                        if sig > 1e-8 then
                            local spot_mom = spot_now
                            local mom_ok_data = true
                            if mom_sec > 0 then
                                spot_mom = spot_at_sec(by_sec, spot.close_sec - mom_sec)
                                if spot_mom == nil or spot_mom <= 0 then
                                    mom_ok_data = false
                                end
                            end
                            if mom_ok_data then
                                local best = nil
                                local legs = {
                                    { token = market.up_token, is_up = true },
                                    { token = market.down_token, is_up = false },
                                }
                                for i = 1, #legs do
                                    local leg = legs[i]
                                    local token = leg.token
                                    local is_up = leg.is_up
                                    local mom_pass = (mom_sec <= 0)
                                        or (is_up and spot_now >= spot_mom)
                                        or ((not is_up) and spot_now <= spot_mom)
                                    local book = (mom_pass and token ~= nil) and bk.book(token) or nil
                                    if book ~= nil and book.fresh == true then
                                        local px = maker and book.best_bid or book.best_ask
                                        local n = tonumber(px)
                                        if n ~= nil and n >= lo and n <= hi then
                                            local ln_r = is_up and math.log(spot_now / spot_open) or math.log(spot_open / spot_now)
                                            local z = ln_r / (sig * math.sqrt(math.max(1, tl + 1)))
                                            local fair = norm_cdf(z)
                                            local fee = maker and 0.0 or taker_fee(n)
                                            local edge = fair - (n + fee)
                                            if edge >= min_edge and (best == nil or edge > best.edge) then
                                                best = { token = token, price = px, edge = edge }
                                            end
                                        end
                                    end
                                end
                                if best ~= nil then
                                    quoted[k] = { token = best.token, price = best.price, filled = false, cancelled = false }
                                    out.entries[#out.entries + 1] = {
                                        token = best.token,
                                        price = best.price,
                                        reason = maker and "late-maker" or "late-taker",
                                    }
                                end
                            end
                        end
                    end
                end
            end
        end
    end
    return out
end
