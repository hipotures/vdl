/** Keep this rule aligned with repository.normalize_url. Never write the clipboard. */
export function normalizeURL(value) {
  if (typeof value !== "string" || !value.trim()) throw new Error("Paste a source URL first.");
  if (value.length > 4096) throw new Error("The URL is too long (maximum 4096 characters).");
  const clean = value.trim().split("?", 1)[0];
  if (/[\s\x00-\x1f\x7f\\]/u.test(clean) || !/^https?:\/\/[^/?#]/iu.test(clean)) {
    throw new Error("Use a complete http:// or https:// URL without spaces.");
  }
  let parsed;
  try { parsed = new URL(clean); } catch { throw new Error("This URL is not valid."); }
  if (!parsed.hostname || parsed.username || parsed.password || clean.split("//", 2)[1].split(/[/?#]/u, 1)[0].includes("@")) throw new Error("Use a URL without embedded credentials.");
  return clean;
}

export function age(timestamp, now = Date.now() / 1000) {
  if (timestamp === null) return "Never checked";
  const seconds = Math.max(0, Math.floor(now - timestamp));
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}
