/**
 * Verificador de atualização (GitHub releases, sem polling).
 *
 * - Chamado UMA vez no mount do App + no botão "verificar" da home.
 * - Falhas silenciam (repo privado/offline/rate-limit = sem popup).
 * - Lógica de asset espelha `site/app/app.vue` (pickMac/pickWin).
 */

export const UPDATE_REPO = "CosmicSnow/GoDrinking";
export const UPDATE_API_URL = `https://api.github.com/repos/${UPDATE_REPO}/releases/latest`;
export const RELEASES_URL = `https://github.com/${UPDATE_REPO}/releases`;
export const RELEASES_LATEST_URL = `${RELEASES_URL}/latest`;
export const UPDATE_TIMEOUT_MS = 8000;

export interface ReleaseAsset {
  id?: number;
  name: string;
  browser_download_url: string;
}

export interface UpdateInfo {
  /** Versão nova normalizada ("0.7.7", sem "v"). */
  latest: string;
  /** Tag original da release ("v0.7.7"). */
  tag: string;
  /** Download direto do asset da plataforma (null = sem asset direto). */
  assetUrl: string | null;
  /** Página da release (fallback quando o repo é privado ou sem asset). */
  releasesUrl: string;
}

/** "v0.7.7" -> "0.7.7"; devolve null quando não parece versão. */
export function normalizeTag(tag: string): string | null {
  const cleaned = tag.trim().replace(/^v/i, "");
  if (!/^\d+\.\d+\.\d+/.test(cleaned)) return null;
  return cleaned.split(/[+-]/)[0];
}

type Triple = [number, number, number];

/** "0.7.7" -> [0,7,7]; sufixos ignorados. */
export function parseVersion(version: string): Triple | null {
  const match = version.trim().match(/^(\d+)\.(\d+)\.(\d+)/);
  if (!match) return null;
  return [Number(match[1]), Number(match[2]), Number(match[3])];
}

/** true quando `latest` é estritamente maior que `current`. */
export function isNewer(current: string, latest: string): boolean {
  const a = parseVersion(current);
  const b = parseVersion(latest);
  if (!a || !b) return false;
  for (let i = 0; i < 3; i++) {
    if (b[i] !== a[i]) return b[i] > a[i];
  }
  return false;
}

export type Platform = "mac" | "win" | "other";

/** Plataforma via userAgent (espelha o site: Windows -> win, resto -> mac). */
export function detectPlatform(userAgent?: string): Platform {
  const ua =
    userAgent ??
    (typeof navigator !== "undefined" ? navigator.userAgent : "");
  if (/windows/i.test(ua)) return "win";
  if (/macintosh|mac os x/i.test(ua)) return "mac";
  return "other";
}

/**
 * Escolhe o asset direto por plataforma (mesma ordem do site):
 * - mac: *.dmg, senão *.zip com "macos" no nome.
 * - win: *setup*.exe, senão *.msi, senão *portable*.exe,
 *   senão qualquer *.exe (cobre o `goDrinking.exe` real).
 * - other: tenta mac primeiro, senão win.
 */
export function pickAsset(assets: ReleaseAsset[], platform: Platform): ReleaseAsset | null {
  const lower = (name: string): string => name.toLowerCase();
  const pickMac =
    assets.find((a) => lower(a.name).endsWith(".dmg")) ??
    assets.find((a) => /macos/i.test(a.name) && lower(a.name).endsWith(".zip")) ??
    null;
  const pickWin =
    assets.find((a) => /setup/i.test(a.name) && lower(a.name).endsWith(".exe")) ??
    assets.find((a) => lower(a.name).endsWith(".msi")) ??
    assets.find((a) => /portable/i.test(a.name) && lower(a.name).endsWith(".exe")) ??
    assets.find((a) => lower(a.name).endsWith(".exe")) ??
    null;
  if (platform === "win") return pickWin;
  if (platform === "mac") return pickMac;
  return pickMac ?? pickWin;
}

export interface CheckOptions {
  fetchFn?: typeof fetch;
  userAgent?: string;
  timeoutMs?: number;
}

/**
 * Busca a latest release e compara com a versão atual.
 * Devolve UpdateInfo quando há versão nova, null caso contrário —
 * NUNCA joga (404/rede/timeout/parse = sem popup).
 */
export async function checkForUpdate(
  current: string,
  opts: CheckOptions = {},
): Promise<UpdateInfo | null> {
  const fetchFn = opts.fetchFn ?? (typeof fetch !== "undefined" ? fetch : null);
  if (!fetchFn) return null;
  const controller =
    typeof AbortController !== "undefined" ? new AbortController() : null;
  const timeoutMs = opts.timeoutMs ?? UPDATE_TIMEOUT_MS;
  const timer =
    controller != null
      ? setTimeout(() => controller.abort(), timeoutMs)
      : null;
  try {
    const res = await fetchFn(UPDATE_API_URL, {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!res.ok) return null;
    const data = (await res.json()) as {
      tag_name?: unknown;
      html_url?: unknown;
      assets?: unknown;
    };
    if (typeof data.tag_name !== "string") return null;
    const latest = normalizeTag(data.tag_name);
    if (!latest || !isNewer(current, latest)) return null;
    const assets = Array.isArray(data.assets)
      ? (data.assets.filter(
          (a): a is ReleaseAsset =>
            typeof a === "object" &&
            a !== null &&
            typeof (a as ReleaseAsset).name === "string" &&
            typeof (a as ReleaseAsset).browser_download_url === "string",
        ))
      : [];
    const platform = detectPlatform(opts.userAgent);
    const asset = pickAsset(assets, platform);
    const releasesUrl =
      typeof data.html_url === "string" && data.html_url.length > 0
        ? data.html_url
        : RELEASES_LATEST_URL;
    return {
      latest,
      tag: data.tag_name,
      assetUrl: asset?.browser_download_url ?? null,
      releasesUrl,
    };
  } catch {
    return null;
  } finally {
    if (timer != null) clearTimeout(timer);
  }
}

/** Abre a URL de download (nova aba no navegador / navegador do SO no Tauri). */
export function openUpdateUrl(url: string): void {
  if (typeof window !== "undefined" && typeof window.open === "function") {
    window.open(url, "_blank", "noopener,noreferrer");
  }
}
