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

  // Redraw charts when the colour scheme changes so they pick up new tokens.
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => { drawPriceChart(); drawEquity(); drawPosts(); });
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
  const v1 = evalFor("v1"), v2 = evalFor("v2");
  const relevant = d.posts.filter((p) => Object.values(p.scores).some((s) => s.relevance >= 0.5)).length;
  const html = [
    card("v1 PnL after costs", usd(v1.full.pnl_after_costs), `${v1.full.fills} fills, holdout ${usd(v1.second_half.pnl_after_costs)}`, signClass(v1.full.pnl_after_costs)),
    card("v2 PnL after costs", usd(v2.full.pnl_after_costs), `${v2.full.fills} fills, holdout ${usd(v2.second_half.pnl_after_costs)}`, signClass(v2.full.pnl_after_costs)),
    card("Costs paid (v2)", usd(v2.full.fees + v2.full.slippage), "4.5 bp taker fee + 1 bp slippage", ""),
    card("Kalshi vetoes (v2)", String(v2.full.pm_vetoes), `${v2.full.prediction_snapshots} ladder snapshots`),
    card("Posts scored by Jev", String(d.posts.length), `${relevant} judged relevant to BTC or ETH`),
  ];
  if (d.jev) html.push(card("Jev latency p50", `${d.jev.latency_ms_p50} ms`, `p99 ${d.jev.latency_ms_p99} ms, ${d.jev.input_tokens_per_post_mean} tokens/post`));
  if (d.paper && d.paper.engine_compute_only) {
    html.push(card("Engine decision time p50", `${d.paper.engine_compute_only.p50_us} us`, `live paper run, ${d.paper.duration_secs}s`));
  }
  $("cards").innerHTML = html.join("");
}

function chartOptions(el) {
  return {
    width: el.clientWidth,
    height: el.clientHeight,
    autoSize: true,
    layout: { background: { type: "solid", color: css("--card") }, textColor: css("--muted"), fontSize: 11 },
    grid: { vertLines: { color: css("--border") }, horzLines: { color: css("--border") } },
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
  chart.subscribeCrosshairMove((param) => {
    if (!param.time) return;
    const p = param.seriesData.get(price);
    const b = bandByTime.get(param.time);
    let html = `<b>${utc(param.time)}</b> ${esc(coin)} ${p ? p.value.toFixed(2) : ""}`;
    if (b) html += ` | Kalshi median ${b[2] ?? "n/a"}, P(close above spot) ${b[4] ?? "n/a"} for the ${utc(b[5]).slice(11)} close`;
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
  for (const [name, color] of [["v1", css("--v1")], ["v2", css("--v2")]]) {
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
  for (const e of state.data.evaluations) {
    for (const r of [e.full, e.first_half, e.second_half]) {
      rows.push(`<tr><td>${esc(e.name)}</td><td>${esc(r.window)}</td><td class="num">${r.fills}</td><td class="num">${pct(r.hit_rate)}</td>` +
        `<td class="num ${signClass(r.pnl_after_costs)}">${usd(r.pnl_after_costs)}</td><td class="num">${usd(r.pnl_before_costs)}</td>` +
        `<td class="num">${usd(r.fees + r.slippage)}</td><td class="num">${usd(r.max_drawdown)}</td><td class="num">${r.pm_vetoes}</td></tr>`);
    }
  }
  table($("evals"), [["strategy"], ["window"], ["fills", "num"], ["hit rate", "num"], ["PnL after costs", "num"], ["before costs", "num"], ["costs", "num"], ["max drawdown", "num"], ["Kalshi vetoes", "num"]], rows);
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
