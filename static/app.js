import { normalizeURL, age } from "./url.js";
import { sortSources } from "./sort.js";

const $ = (selector, root = document) => root.querySelector(selector);
const input = $("#source-url"), add = $("#add"), preview = $("#url-preview");
const cards = new Map(), pendingActions = new Set();
let adding = false, lastState = null, lastTime = -Infinity;
let inFlight = null, timer = null, epoch = 0;
let sortMode = "date";

function setSortMode(mode) {
  sortMode = mode;
  document.querySelectorAll("#sort button").forEach((button) => {
    button.setAttribute("aria-pressed", String(button.dataset.sort === mode));
  });
  if (lastState) renderState(lastState);
}

$("#sort").addEventListener("click", (event) => {
  const mode = event.target.closest("button")?.dataset.sort;
  if (mode) setSortMode(mode);
});

function message(text, error = false) {
  $("#message").textContent = text;
  $("#message").dataset.error = String(error);
}

function updatePreview() {
  try {
    const clean = normalizeURL(input.value);
    preview.textContent = `Will save: ${clean}`;
    preview.title = clean;
    preview.dataset.error = "false";
    add.disabled = adding;
  } catch (error) {
    preview.textContent = input.value ? error.message : "Tracking parameters are removed before saving.";
    preview.removeAttribute("title");
    preview.dataset.error = String(Boolean(input.value));
    add.disabled = true;
  }
}

function pasted(text) {
  try {
    input.value = normalizeURL(text);
    message("URL cleaned. Confirm with Add source.");
  } catch (error) {
    input.value = text.slice(0, 4096);
    message(error.message, true);
  }
  updatePreview();
  input.setSelectionRange(0, 0); // Show the profile, not the end of a long URL.
}

input.addEventListener("input", updatePreview);
input.addEventListener("paste", (event) => {
  const text = event.clipboardData?.getData("text/plain");
  if (text) { event.preventDefault(); pasted(text); }
});
$("#paste").addEventListener("click", async () => {
  if (window.isSecureContext && navigator.clipboard?.readText) {
    try { pasted(await navigator.clipboard.readText()); return; }
    catch { /* Denied permission is normal; keep manual paste available. */ }
  }
  input.focus({ preventScroll: true });
  message("Touch and hold the URL field, then choose Paste (or use Ctrl+V). Clipboard access needs HTTPS.");
});

async function request(path, body) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 10000);
  try {
    const response = await fetch(path, {
      method: body === undefined ? "GET" : "POST", cache: "no-store",
      credentials: "same-origin", signal: controller.signal,
      headers: body === undefined ? {} : { "Content-Type": "application/json", "X-VDL-Request": "1" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const data = await response.json();
    if (!response.ok) throw new Error(data.error || `Request failed (${response.status}).`);
    return data;
  } catch (error) {
    if (error.name === "AbortError" || error instanceof TypeError) {
      throw new Error("Cannot reach vdl. Check the connection and try again; duplicate adds are safe.");
    }
    throw error;
  } finally { clearTimeout(timeout); }
}

function renderState(state) {
  if (state.time < lastTime) return; // A slower response must not undo a newer mutation.
  lastTime = state.time; lastState = state;
  const container = $("#sources"), keep = new Set();
  sortSources(state.sources, sortMode).forEach((row, index) => {
    keep.add(row.id);
    let card = cards.get(row.id);
    if (!card) {
      card = $("#source-template").content.firstElementChild.cloneNode(true);
      card.dataset.id = row.id;
      $(".download", card).addEventListener("click", () => act(row.id, "download-now"));
      $(".disable", card).addEventListener("click", () => { $(".confirm", card).hidden = false; });
      $(".cancel-disable", card).addEventListener("click", () => { $(".confirm", card).hidden = true; });
      $(".confirm-disable", card).addEventListener("click", () => act(row.id, "disable"));
      cards.set(row.id, card);
    }
    // Preserve DOM nodes, focus, open confirmations and scroll position.
    if (container.children[index] !== card) container.insertBefore(card, container.children[index] || null);
    card.dataset.state = row.state;
    $(".service", card).textContent = row.service;
    $(".account", card).textContent = row.account;
    $(".badge", card).textContent = { downloading: "Downloading", finishing: "Finishing · disabled", queued: "Pending", active: "Active", inactive: "Disabled" }[row.state];
    $(".last-check", card).textContent = `Last attempt: ${age(row.last_check, state.time)}`;
    const busy = pendingActions.has(row.id);
    $(".download", card).disabled =
      busy || !row.active || row.state === "downloading" || row.download_requested;
    $(".disable", card).disabled = busy || !row.active;
    $(".confirm-disable", card).disabled = busy;
    if (!row.active) $(".confirm", card).hidden = true;
  });
  for (const [id, card] of cards) if (!keep.has(id)) { card.remove(); cards.delete(id); }
  $("#count").textContent = String(state.sources.length);
  $("#empty").hidden = state.sources.length > 0;
  $("#activity").textContent = state.download
    ? `Downloading ${state.download} · ${state.pending} pending`
    : state.owner_running ? `Ready · ${state.pending} pending`
      : `Downloader stopped · ${state.pending} pending. You can still add sources; start vdl to process them.`;
}

async function act(id, action) {
  if (pendingActions.has(id)) return;
  pendingActions.add(id); epoch += 1;
  if (lastState) renderState(lastState);
  try {
    const data = await request(`/api/sources/${action}`, { id });
    renderState(data.state);
    message(action === "disable" ? `Disabled ${id}. Existing files are kept.` : `Download requested: ${id}.`);
  } catch (error) { message(error.message, true); }
  finally { pendingActions.delete(id); if (lastState) renderState(lastState); }
}

$("#add-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (adding) return;
  let url;
  try { url = normalizeURL(input.value); } catch (error) { message(error.message, true); return; }
  const submitted = input.value;
  adding = true; epoch += 1; add.textContent = "Adding…"; updatePreview();
  try {
    const data = await request("/api/sources", { url });
    renderState(data.state);
    const next = data.state.download ? "It will run after the current download."
      : data.state.owner_running ? "Ready for the first check." : "Start vdl to process it.";
    message(data.created ? `Added ${data.id}. ${next}`
      : `Already saved: ${data.id}${data.active ? "." : " (disabled)."}`);
    if (input.value === submitted) input.value = "";
  } catch (error) { message(error.message, true); }
  finally { adding = false; add.textContent = "Add source"; updatePreview(); }
});

async function refresh() {
  if (inFlight) return inFlight;
  const startedAt = epoch;
  inFlight = (async () => {
    try {
      const state = await request("/api/state");
      if (startedAt === epoch) renderState(state);
      $("#connection").textContent = "Live · 1s";
      $("#connection").dataset.offline = "false";
    } catch {
      $("#connection").textContent = "Offline · retrying";
      $("#connection").dataset.offline = "true";
    } finally { inFlight = null; }
  })();
  return inFlight;
}

async function tick() {
  clearTimeout(timer);
  await refresh();
  clearTimeout(timer);
  if (!document.hidden) timer = setTimeout(tick, 1000);
}
document.addEventListener("visibilitychange", () => { clearTimeout(timer); if (!document.hidden) tick(); });
window.addEventListener("online", tick);
window.addEventListener("pageshow", tick);
updatePreview(); tick();
