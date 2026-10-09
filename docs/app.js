// mft-engine replay demo. Reads data/demo.json (written by `mft-engine
// export-demo`) and draws it. Computes nothing about the strategy itself.
"use strict";

const state = { data: null, coin: "BTC", strategy: "v2", price: null, equity: null };

const $ = (id) => document.getElementById(id);
const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const usd = (x) => (x == null ? "n/a" : (x < 0 ? "-$" : "$") + Math.abs(x).toFixed(2));
const pct = (x) => (x == null ? "n/a" : (x * 100).toFixed(1) + "%");
const utc = (t) => new Date(t * 1000).toISOString().replace("T", " ").slice(0, 16) + " UTC";
const signClass = (x) => (x < 0 ? "neg" : x > 0 ? "pos" : "");

async function main() {
  try {
    const res = await fetch("data/demo.json");
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    state.data = await res.json();
  } catch (e) {
    $("cards").textContent = `Could not load data/demo.json (${e.message}). Serve the docs folder over HTTP, e.g. python -m http.server.`;
    return;
  }
  const d = state.data;
  $("notice").textContent = d.notice;
  $("generated").textContent = d.generated_at;
  $("window").textContent = `Replay window ${d.window.start} to ${d.window.end}.`;

  const coins = Object.keys(d.bars);
  $("coin").innerHTML = coins.map((c) => `<option>${esc(c)}</option>`).join("");
  state.coin = coins[0];
  $("coin").addEventListener("change", (e) => { state.coin = e.target.value; drawPriceChart(); drawPosts(); drawDecisions(); });
  $("strategy").addEventListener("change", (e) => { state.strategy = e.target.value; drawPriceChart(); drawDecisions(); });

  drawCards();
  drawPriceChart();
  drawPosts();
  drawEquity();
  drawEvalTable();
  drawDecisions();
  drawLedger();
  drawLatency();
  drawCrowd();
  drawUniverse();
  drawCarry();

  // Light is the default; the nav toggle switches to dark and redraws the
  // charts so they pick up the new colour tokens.
  $("theme-toggle").addEventListener("click", () => {
    const dark = document.documentElement.getAttribute("data-theme") !== "dark";
    if (dark) document.documentElement.setAttribute("data-theme", "dark");
    else document.documentElement.removeAttribute("data-theme");
    try { localStorage.setItem("mft-theme", dark ? "dark" : "light"); } catch (e) { /* storage unavailable */ }
    drawPriceChart(); drawEquity(); drawPosts(); drawCrowd(); drawCarry();
  });
  window.addEventListener("resize", () => drawPosts());
}

function evalFor(name) {
  return state.data.evaluations.find((e) => e.name === name);
}

function card(label, value, sub, cls = "") {
  return `<div class="card"><div class="label">${esc(label)}</div><div class="value ${cls}">${esc(value)}</div><div class="sub">${esc(sub)}</div></div>`;
}

function drawCards() {
  const d = state.data;
  const v2 = evalFor("v2");
  const relevant = d.posts.filter((p) => Object.values(p.scores).some((s) => s.relevance >= 0.5)).length;
  const html = [];
  if (d.paper && d.paper.engine_compute_only) {
    html.push(card("Engine decision time", `${d.paper.engine_compute_only.p50_us} µs`, `p50 on the live feed, p99 ${d.paper.engine_compute_only.p99_us} µs`));
    html.push(card("Feed frame to decision", `${(d.paper.event_to_decision_all_events.p50_us / 1000).toFixed(2)} ms`, `p50 over ${d.paper.event_to_decision_all_events.samples} live events`));
  }
  if (d.jev) html.push(card("Jev reasoning per post", `${d.jev.latency_ms_p50} ms`, `p50, p99 ${d.jev.latency_ms_p99} ms, ${Math.round(d.jev.input_tokens_per_post_mean)} tokens`));
  html.push(card("Kalshi ladder snapshots", v2.full.prediction_snapshots.toLocaleString(), "minute-level implied distributions, BTC and ETH"));
  const pa = d.polymarket && d.polymarket.agreement;
  if (pa && pa.rate != null) html.push(card("Kalshi and Polymarket agree", `${(pa.rate * 100).toFixed(0)}%`, `of ${pa.both.toLocaleString()} bars with both readings, on the side of P(up) vs 0.5`));
  html.push(card("Posts reasoned over", String(d.posts.length), `${relevant} judged relevant to BTC or ETH`));
  html.push(card("Experiments on the ledger", String(d.ledger.length), "hash-chained, kept and killed alike"));
  html.push(card("Deterministic replay", v2.full.decisions_fingerprint.slice(0, 8), "decision fingerprint, identical on every run"));
  html.push(card("Live sources, one loop", "3", "Hyperliquid, Kalshi, Hacker News"));
  $("cards").innerHTML = html.join("");
}

function chartOptions(el) {
  return {
    width: el.clientWidth,
    height: el.clientHeight,
    autoSize: true,
    layout: { background: { type: "solid", color: css("--card") }, textColor: css("--muted"), fontSize: 12,
      fontFamily: getComputedStyle(document.body).fontFamily },
    grid: { vertLines: { visible: false }, horzLines: { color: css("--grid") } },
    rightPriceScale: { borderColor: css("--border") },
    timeScale: { borderColor: css("--border"), timeVisible: true, secondsVisible: false },
    crosshair: { mode: 0 },
  };
}

// Keep the last point for each time, sorted, as the chart library requires.
function uniqueByTime(points) {
  const m = new Map();
  for (const p of points) m.set(p.time, p);
  return [...m.values()].sort((a, b) => a.time - b.time);
}

function drawPriceChart() {
  const d = state.data, coin = state.coin;
  const el = $("price");
  if (state.price) state.price.remove();
  const chart = LightweightCharts.createChart(el, chartOptions(el));
  state.price = chart;

  const price = chart.addLineSeries({ color: css("--price"), lineWidth: 1.5, priceLineVisible: false, lastValueVisible: true });
  price.setData(uniqueByTime(d.bars[coin].map(([t, c]) => ({ time: t, value: c }))));

  const band = d.pm_band[coin] || [];
  const line = (i) => uniqueByTime(band.map((r) => (r[i] == null ? { time: r[0] } : { time: r[0], value: r[i] })));
  const opts = (color, style) => ({ color, lineWidth: 1, lineStyle: style, priceLineVisible: false, lastValueVisible: false, crosshairMarkerVisible: false });
  chart.addLineSeries(opts(css("--band"), 0)).setData(line(1));
  chart.addLineSeries(opts(css("--band"), 0)).setData(line(3));
  chart.addLineSeries(opts(css("--median"), 2)).setData(line(2));

  const decisions = d.decisions[state.strategy].filter((x) => x.coin === coin);
  const markers = decisions.map((x) => x.kind === "fill"
    ? { time: x.t, position: x.side === "buy" ? "belowBar" : "aboveBar", color: x.side === "buy" ? css("--up") : css("--down"), shape: x.side === "buy" ? "arrowUp" : "arrowDown" }
    : { time: x.t, position: "aboveBar", color: css("--warn"), shape: "circle", size: 0.6 });
  price.setMarkers(markers.sort((a, b) => a.time - b.time));

  const byTime = new Map(decisions.map((x) => [x.t, x]));
  const bandByTime = new Map(band.map((r) => [r[0], r]));
  const polyByTime = new Map(((d.polymarket && d.polymarket.p_up && d.polymarket.p_up[coin]) || []).map((r) => [r[0], r[1]]));
  chart.subscribeCrosshairMove((param) => {
    if (!param.time) return;
    const p = param.seriesData.get(price);
    const b = bandByTime.get(param.time);
    let html = `<b>${utc(param.time)}</b> ${esc(coin)} ${p ? p.value.toFixed(2) : ""}`;
    if (b) html += ` | Kalshi median ${b[2] ?? "n/a"}, P(close above spot) ${b[4] ?? "n/a"} for the ${utc(b[5]).slice(11)} close`;
    const pp = polyByTime.get(param.time);
    if (pp != null) html += ` | Polymarket P(above spot at noon ET) ${pp}`;
    const x = byTime.get(param.time);
    if (x) {
      html += `<br>${x.kind === "fill" ? `<span class="${x.side === "buy" ? "pos" : "neg"}">${x.side.toUpperCase()} ${Math.abs(x.qty).toFixed(5)} @ ${x.px}</span>` : `<span style="color:var(--warn)">BLOCKED: ${esc(x.reason)}</span>`}`;
      html += ` | target ${x.target.toFixed(0)} USD (strategy wanted ${x.raw_target.toFixed(0)}) | ${esc(x.why)}`;
    }
    $("hover").innerHTML = html;
  });
  chart.timeScale().fitContent();
}

function drawPosts() {
  const d = state.data, coin = state.coin;
  const svg = $("posts");
  const w = svg.clientWidth || 600, h = svg.clientHeight || 90;
  const t0 = d.bars[coin][0][0], t1 = d.bars[coin][d.bars[coin].length - 1][0];
  const x = (t) => 8 + ((t - t0) / (t1 - t0)) * (w - 16);
  const parts = [`<line x1="8" x2="${w - 8}" y1="${h / 2}" y2="${h / 2}" stroke="${css("--border")}"/>`];
  // Day ticks.
  for (let t = Math.ceil(t0 / 86400) * 86400; t < t1; t += 86400) {
    parts.push(`<line x1="${x(t)}" x2="${x(t)}" y1="6" y2="${h - 6}" stroke="${css("--border")}" stroke-dasharray="2 3"/>`);
    parts.push(`<text x="${x(t) + 3}" y="14" font-size="10" fill="${css("--muted")}">${new Date(t * 1000).toISOString().slice(5, 10)}</text>`);
  }
  d.posts.forEach((p, i) => {
    if (p.t < t0 || p.t > t1) return;
    const s = p.scores[coin] || { relevance: 0, bullish: 0, bearish: 0 };
    const color = s.relevance < 0.5 ? css("--neutral") : s.bullish > s.bearish ? css("--up") : s.bearish > s.bullish ? css("--down") : css("--neutral");
    const r = 2.5 + 6 * s.relevance;
    const y = h / 2 + (s.relevance >= 0.5 ? -14 : 10) + ((i * 7) % 11) - 5;
    parts.push(`<circle data-i="${i}" cx="${x(p.t).toFixed(1)}" cy="${y}" r="${r.toFixed(1)}" fill="${color}" fill-opacity="${s.relevance >= 0.5 ? 0.9 : 0.45}"/>`);
  });
  svg.innerHTML = parts.join("");

  const tip = $("tip");
  svg.onmousemove = (e) => {
    const i = e.target.getAttribute && e.target.getAttribute("data-i");
    if (i == null) { tip.hidden = true; return; }
    const p = d.posts[+i];
    tip.innerHTML = postHtml(p);
    tip.hidden = false;
    tip.style.left = Math.min(e.clientX + 12, window.innerWidth - tip.offsetWidth - 8) + "px";
    tip.style.top = e.clientY + 14 + "px";
  };
  svg.onmouseleave = () => { tip.hidden = true; };
  svg.onclick = (e) => {
    const i = e.target.getAttribute && e.target.getAttribute("data-i");
    if (i == null) return;
    const p = d.posts[+i];
    $("post-detail").innerHTML = postHtml(p) + ` <a href="${esc(p.url)}" target="_blank" rel="noopener">open on Hacker News</a>`;
    state.price.timeScale().setVisibleRange({ from: p.t - 3 * 3600, to: p.t + 3 * 3600 });
  };
}

function postHtml(p) {
  const rows = Object.entries(p.scores).map(([c, s]) =>
    `${esc(c)}: relevance ${s.relevance.toFixed(2)}, bullish ${s.bullish.toFixed(2)}, bearish ${s.bearish.toFixed(2)}, novelty ${s.novelty.toFixed(2)}`).join("<br>");
  return `<b>${esc(p.source)} ${esc(p.kind)}</b>, published ${utc(p.published)}, usable from ${utc(p.t)}<br>` +
    `"${esc(p.text)}"<br><span class="muted">${esc(p.scorer)}:</span><br>${rows}`;
}

function drawEquity() {
  const d = state.data;
  const el = $("equity");
  if (state.equity) state.equity.remove();
  const chart = LightweightCharts.createChart(el, chartOptions(el));
  state.equity = chart;
  for (const [name, color] of [["v1", css("--v1")], ["v2", css("--v2")], ["v2b", css("--v2b")]]) {
    if (!d.equity[name]) continue;
    const s = chart.addLineSeries({ color, lineWidth: 2, priceLineVisible: false });
    s.setData(uniqueByTime(d.equity[name].map(([t, e]) => ({ time: t, value: e }))));
  }
  chart.timeScale().fitContent();
}

function table(el, head, rows) {
  el.innerHTML = `<thead><tr>${head.map(([h, cls]) => `<th class="${cls || ""}">${esc(h)}</th>`).join("")}</tr></thead>` +
    `<tbody>${rows.join("")}</tbody>`;
}

function drawEvalTable() {
  const rows = [];
  const evals = [...state.data.evaluations];
  if (state.data.polymarket && state.data.polymarket.v2b) evals.push(state.data.polymarket.v2b);
  for (const e of evals) {
    for (const r of [e.full, e.first_half, e.second_half]) {
      rows.push(`<tr><td>${esc(e.name)}</td><td>${esc(r.window)}</td><td class="num">${r.fills}</td><td class="num">${pct(r.hit_rate)}</td>` +
        `<td class="num ${signClass(r.pnl_after_costs)}">${usd(r.pnl_after_costs)}</td><td class="num">${usd(r.pnl_before_costs)}</td>` +
        `<td class="num">${usd(r.fees + r.slippage)}</td><td class="num">${usd(r.max_drawdown)}</td><td class="num">${r.pm_vetoes}</td></tr>`);
    }
  }
  table($("evals"), [["strategy"], ["window"], ["fills", "num"], ["hit rate", "num"], ["PnL after costs", "num"], ["before costs", "num"], ["costs", "num"], ["max drawdown", "num"], ["market vetoes", "num"]], rows);
}

function drawDecisions() {
  const all = state.data.decisions[state.strategy].filter((x) => x.coin === state.coin);
  const blocks = all.filter((x) => x.kind === "blocked");
  const reasons = {};
  for (const b of blocks) { const k = b.reason.split(":")[0]; reasons[k] = (reasons[k] || 0) + 1; }
  $("decision-summary").textContent = `${state.strategy}, ${state.coin}: ${all.length - blocks.length} fills, ${blocks.length} blocked orders` +
    (blocks.length ? ` (${Object.entries(reasons).map(([k, v]) => `${k}: ${v}`).join(", ")})` : "") + ". Showing the first 300.";
  const rows = all.slice(0, 300).map((x) => `<tr><td>${utc(x.t)}</td>` +
    (x.kind === "fill"
      ? `<td class="${x.side === "buy" ? "pos" : "neg"}">${x.side}</td><td class="num">${Math.abs(x.qty).toFixed(5)}</td><td class="num">${x.px}</td><td>filled on paper</td>`
      : `<td style="color:var(--warn)">blocked</td><td class="num">${Math.abs(x.qty).toFixed(5)}</td><td class="num"></td><td>${esc(x.reason)}</td>`) +
    `<td>${esc(x.why)}</td></tr>`);
  table($("decisions"), [["time"], ["action"], ["qty", "num"], ["price", "num"], ["risk"], ["why"]], rows);
}

function drawLedger() {
  const rows = state.data.ledger.map((e) => `<tr><td class="num">${e.seq}</td><td>${esc(e.variant)}</td><td><span class="tag ${esc(e.verdict)}">${esc(e.verdict)}</span></td>` +
    `<td class="num ${signClass(e.full_pnl)}">${usd(e.full_pnl)}</td><td class="num ${signClass(e.holdout_pnl)}">${usd(e.holdout_pnl)}</td><td class="num">${e.fills}</td>` +
    `<td>${esc(e.hypothesis)}</td><td><code>${esc(e.prev)}</code> to <code>${esc(e.hash)}</code></td></tr>`);
  const count = (v) => state.data.ledger.filter((e) => e.verdict === v).length;
  const stat = (n, l) => `<div class="stat"><span class="n">${esc(n)}</span><span class="l">${esc(l)}</span></div>`;
  $("ledger-stats").innerHTML = [
    stat(state.data.ledger.length, "entries on the chain"),
    stat(count("killed"), "ideas killed, still on the record"),
    stat(count("kept"), "ideas kept (passed the holdout)"),
    stat(count("preregistered"), "specs pre-registered before a run"),
  ].join("");
  table($("ledger"), [["#", "num"], ["variant"], ["verdict"], ["PnL", "num"], ["holdout PnL", "num"], ["fills", "num"], ["hypothesis"], ["hash chain"]], rows);
}

function drawLatency() {
  const p = state.data.paper;
  if (!p) { $("latency").innerHTML = "<tr><td>No paper run recorded.</td></tr>"; return; }
  const row = (label, s) => `<tr><td>${esc(label)}</td><td class="num">${s.samples}</td><td class="num">${s.p50_us ?? "n/a"}</td><td class="num">${s.p99_us ?? "n/a"}</td></tr>`;
  const rows = [
    row("websocket frame read to decision, all events", p.event_to_decision_all_events),
    row("same, bar events only (the ones that can trade)", p.event_to_decision_bar_events),
    row("inside Engine::on_event only", p.engine_compute_only),
  ];
  table($("latency"), [[`live paper run ${p.started}, ${p.duration_secs}s, strategy ${p.strategy}`], ["samples", "num"], ["p50 (us)", "num"], ["p99 (us)", "num"]], rows);
}

main();

function drawCrowd() {
  const session = state.data.session;
  const el = $("crowd");
  if (!session || !session.positioning || Object.keys(session.positioning).length === 0) {
    el.outerHTML = '<p class="muted small">No live session with positioning was exported.</p>';
    return;
  }
  $("session-window").textContent = `Recorded session ${session.window.start} to ${session.window.end}` +
    (session.window.warmup_bars ? ", replayed after the 90 one-minute bars before it so momentum starts warm." : ".");
  if (state.crowd) state.crowd.remove();
  const chart = LightweightCharts.createChart(el, chartOptions(el));
  state.crowd = chart;
  const colors = [css("--v2"), css("--median"), css("--warn")];
  const legend = [];
  Object.entries(session.positioning).forEach(([coin, rows], i) => {
    const s = chart.addLineSeries({ color: colors[i % colors.length], lineWidth: 2, priceLineVisible: false });
    s.setData(uniqueByTime(rows.map(([t, share]) => ({ time: t, value: +(share * 100).toFixed(1) }))));
    legend.push(`<span><i class="sw" style="background:${colors[i % colors.length]}"></i>${esc(coin)} % long by value</span>`);
  });
  const sixty = chart.addLineSeries({ color: css("--muted"), lineWidth: 1, lineStyle: 2, priceLineVisible: false, lastValueVisible: false });
  const anyRows = Object.values(session.positioning)[0];
  sixty.setData(uniqueByTime(anyRows.map(([t]) => ({ time: t, value: 60 }))));
  legend.push('<span><i class="sw" style="background:var(--muted)"></i>60% threshold</span>');
  $("crowd-legend").innerHTML = legend.join("");
  chart.timeScale().fitContent();

  // Say plainly when the recorded share never moved, so a flat line reads as data.
  const flat = Object.entries(session.positioning).filter(([, r]) => r.length && r.every(([, v]) => v === r[0][1]));
  const crowdVetoes = (session.evaluations || []).reduce((n, r) => n + (r.crowd_vetoes || 0), 0);
  if (flat.length) {
    $("crowd-caption").textContent = `Flat by observation, not by construction: over this ${Math.round((Date.parse(session.window.end) - Date.parse(session.window.start)) / 60000)}-minute session ` +
      `the top wallets' long share did not change between snapshots (${flat.map(([c, r]) => `${c} ${(r[0][1] * 100).toFixed(1)}%`).join(", ")}). ` +
      (flat.every(([, r]) => r[0][1] > 0.6) ? `All sit above the 60% threshold, so v3 could only go long here` + (crowdVetoes ? `; it vetoed ${crowdVetoes} entries on the short side.` : ".") : "");
  }

  const rows = (session.evaluations || []).map((r, i) => `<tr><td>${["v1", "v2", "v3"][i]}</td><td class="num">${r.bars}</td>` +
    `<td class="num">${r.positioning_snapshots}</td><td class="num">${r.prediction_snapshots}</td><td class="num">${r.fills}</td>` +
    `<td class="num ${signClass(r.pnl_after_costs)}">${usd(r.pnl_after_costs)}</td><td class="num">${r.pm_vetoes + r.crowd_vetoes}</td></tr>`);
  table($("session-evals"), [["strategy on the recorded session"], ["bars", "num"], ["positioning snapshots", "num"], ["Kalshi snapshots", "num"], ["fills", "num"], ["PnL after costs", "num"], ["gate vetoes", "num"]], rows);
}

function drawUniverse() {
  const u = state.data.universe;
  if (!u) { $("universe-summary").textContent = "Universe not exported."; return; }
  const dexes = Object.entries(u.per_dex).map(([d, n]) => `${d}: ${n}`).join(", ");
  $("universe-summary").textContent = `${u.live_contracts} live perps on ${Object.keys(u.per_dex).length} of ${u.dexes_listed} listed dexes (${dexes}), ` +
    `fetched ${u.fetched_at}. Builder (HIP-3) dexes such as xyz list equities, indices and commodities. Funding is hourly, annualised here. Top 20 by 24h volume:`;
  const n = (x, d) => (x == null ? "n/a" : Number(x).toFixed(d));
  const rows = u.top_by_volume.map((c) => `<tr><td>${esc(c.coin)}</td><td>${esc(c.dex)}</td><td class="num">${n(c.mark, 2)}</td>` +
    `<td class="num">${n(c.volume_musd, 1)}</td><td class="num">${n(c.oi_musd, 1)}</td><td class="num">${n(c.funding_pct_year, 1)}</td>` +
    `<td>${esc(c.zone)}</td><td class="num">${c.max_leverage}x</td></tr>`);
  table($("universe"), [["coin"], ["dex"], ["mark", "num"], ["24h vol $M", "num"], ["OI $M", "num"], ["funding %/yr", "num"], ["zone"], ["max lev", "num"]], rows);
}

function drawCarry() {
  const c = state.data.carry;
  const section = $("carry-equity");
  if (!c) { $("carry-protocol").textContent = "v4 was not exported."; section.hidden = true; return; }
  const w = c.windows;
  $("carry-protocol").textContent = `Specification written to the ledger (#${c.preregistered.seq}, ${c.preregistered.recorded_at}) before any v4 result existed. ` +
    `Universe: ${c.universe_rule} (${c.universe.length} coins: ${c.universe.join(", ")}). ` +
    `Formation ${w.formation[0]} to ${w.formation[1]}; design (in-sample) ${w.in_sample[0]} to ${w.in_sample[1]}; sealed out-of-sample ${w.out_of_sample[0]} to ${w.out_of_sample[1]}, run once. ${c.capital_note}`;

  const p = c.preregistered.config;
  const spec = [
    ["signal", `mean of the last ${p.lookback_hours} settled hourly funding rates`],
    ["rebalance", `every ${p.rebalance_every_hours} h at 00:00 UTC`],
    ["buckets", `short the top ${p.bucket_size}, long the bottom ${p.bucket_size}`],
    ["weights", p.weighting === "equal" ? "equal inside each side" : "inverse volatility inside each side"],
    ["gross", `${usd(p.gross_notional)} (half long, half short)`],
    ["costs", `${p.fills.taker_fee_bps} bp taker fee + ${p.fills.slippage_bps} bp slippage per fill; trades under ${usd(p.min_trade_notional)} skipped`],
    ["kill rule", c.kill_rule],
  ];
  // Anything the sealed run used that differs from the pre-registration was
  // a logged in-sample design choice; show it next to the original.
  const used = (c.oos || c.in_sample || {}).config;
  if (used) {
    const changed = Object.keys(p).filter((k) => JSON.stringify(p[k]) !== JSON.stringify(used[k]));
    spec.push(["changed in-sample (logged)", changed.length ? changed.map((k) => `${k}: ${JSON.stringify(p[k])} to ${JSON.stringify(used[k])}`).join("; ") : "nothing"]);
  }
  table($("carry-spec"), [["pre-registered"], ["value"]], spec.map(([k, v]) => `<tr><td>${esc(k)}</td><td>${esc(v)}</td></tr>`));

  const choices = c.entries.filter((e) => e.verdict === "design_choice");
  const oos = c.oos;
  if (oos) {
    const killed = oos.verdict.endsWith("killed");
    $("carry-verdict").innerHTML = `<b>Sealed out-of-sample: <span class="${killed ? "neg" : "pos"}">${killed ? "killed" : "kept (not refuted)"}</span></b> ` +
      `(ledger #${oos.seq}, fingerprint <code>${esc(oos.report.fingerprint)}</code>). ` +
      `${choices.length} logged in-sample design choice${choices.length === 1 ? "" : "s"}` +
      (choices.length ? `: ${choices.map((e) => esc(e.note).replace(/\.+$/, "")).join("; ")}` : "") + "." +
      (c.oos_reruns ? ` ${c.oos_reruns} forced rerun(s) recorded on the ledger.` : "");
  } else {
    $("carry-verdict").textContent = "The sealed out-of-sample window has not been run yet.";
  }

  const rows = [];
  const row = (label, r) => {
    if (!r) return;
    const ci = r.mean_daily_ci95 ? `${usd(r.mean_daily_ci95[0])} to ${usd(r.mean_daily_ci95[1])}` : "n/a";
    rows.push(`<tr><td>${esc(label)}</td><td class="num">${r.days}</td><td class="num ${signClass(r.pnl_net)}">${usd(r.pnl_net)}</td>` +
      `<td class="num">${usd(r.funding_pnl)}</td><td class="num">${usd(-(r.fees + r.slippage))}</td><td class="num">${usd(r.price_pnl)}</td>` +
      `<td class="num">${r.sharpe_annualised == null ? "n/a" : r.sharpe_annualised.toFixed(2)}</td><td class="num">${r.annualised_return_pct.toFixed(1)}%</td>` +
      `<td class="num">${usd(r.max_drawdown)}</td><td class="num">${r.turnover_per_day.toFixed(2)}</td><td class="num">${usd(r.mean_daily_pnl)}</td><td class="num">${ci}</td></tr>`);
  };
  if (c.in_sample) { row("v4 in-sample", c.in_sample.report); row("hold BTC, in-sample", c.in_sample.benchmark); }
  if (oos) { row("v4 sealed out-of-sample", oos.report); row("hold BTC, out-of-sample", oos.benchmark); }
  table($("carry-results"), [["window"], ["days", "num"], ["net P&L", "num"], ["funding", "num"], ["fees + slippage", "num"], ["price", "num"],
    ["Sharpe", "num"], ["annualised", "num"], ["max drawdown", "num"], ["turnover / day", "num"], ["mean / day", "num"], ["95% block-bootstrap CI, mean / day", "num"]], rows);

  if (state.carry) state.carry.remove();
  const chart = LightweightCharts.createChart(section, chartOptions(section));
  state.carry = chart;
  const add = (points, color) => {
    if (!points) return;
    const s = chart.addLineSeries({ color, lineWidth: 2, priceLineVisible: false });
    s.setData(uniqueByTime(points.map(([t, e]) => ({ time: t, value: e }))));
  };
  const curves = c.curves || {};
  if (curves.in_sample) { add(curves.in_sample.strategy, css("--v2")); add(curves.in_sample.benchmark, css("--v1")); }
  if (curves.out_of_sample) { add(curves.out_of_sample.strategy, css("--median")); add(curves.out_of_sample.benchmark, css("--v1")); }
  chart.timeScale().fitContent();

  const last = (curves.out_of_sample || curves.in_sample || {}).last_rebalance;
  if (last) {
    const fmt = (xs) => xs.map(([coin, f]) => `${esc(coin)} ${(f * 24 * 365 * 100).toFixed(1)}%`).join(", ");
    $("carry-book").innerHTML = `Last rebalance shown, ${utc(last.ts / 1000)}, trailing funding annualised (hourly rate x 24 x 365). ` +
      `Short: ${fmt(last.shorts)}. Long: ${fmt(last.longs)}. Each window starts flat and closes everything at its end, with costs; equity starts at $0.`;
  }
}
