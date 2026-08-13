import { net } from "electron";
import { randomUUID } from "node:crypto";
import {
  isRetryableWebDavRequest,
  isRetryableWebDavStatus,
  webDavRetryDelayMs,
} from "./webdavRetry";

export class WebDavError extends Error {
  readonly status: number;
  readonly retryable: boolean;
  readonly retryAfterMs: number | null;

  constructor(status: number, message: string, retryAfterMs: number | null = null) {
    super(message);
    this.name = "WebDavError";
    this.status = status;
    this.retryable = isRetryableWebDavStatus(status);
    this.retryAfterMs = retryAfterMs;
  }
}

export type WebDavResponse = {
  status: number;
  etag: string | null;
  body: Buffer;
};

const MAX_RESPONSE_BYTES = 2 * 1024 * 1024 * 1024;
const MAX_REQUEST_ATTEMPTS = 3;

const parseRetryAfterMs = (value: string | null): number | null => {
  if (!value) return null;
  const seconds = Number(value);
  if (Number.isFinite(seconds) && seconds >= 0) return seconds * 1_000;
  const date = Date.parse(value);
  return Number.isFinite(date) ? Math.max(0, date - Date.now()) : null;
};

const wait = async (delayMs: number): Promise<void> => {
  await new Promise<void>((resolve) => setTimeout(resolve, delayMs));
};

const normalizeEndpoint = (endpoint: string, allowInsecureHttp: boolean): URL => {
  let url: URL;
  try {
    url = new URL(endpoint);
  } catch {
    throw new Error("WebDAV endpoint must be a valid URL");
  }
  if (url.protocol !== "https:" && !(allowInsecureHttp && url.protocol === "http:")) {
    throw new Error("WebDAV requires HTTPS; enable the explicit insecure HTTP option to override");
  }
  url.hash = "";
  url.search = "";
  url.pathname = url.pathname.replace(/\/+$/, "");
  return url;
};

const appendPath = (base: URL, segments: string[]): string => {
  const url = new URL(base.toString());
  const encoded = segments
    .flatMap((segment) => segment.split(/[\\/]+/))
    .filter(Boolean)
    .map((segment) => encodeURIComponent(segment));
  url.pathname = `${url.pathname.replace(/\/+$/, "")}/${encoded.join("/")}`;
  return url.toString();
};

export class WebDavClient {
  private readonly base: URL;
  private readonly authHeader: string;

  constructor(
    endpoint: string,
    username: string,
    password: string,
    allowInsecureHttp: boolean
  ) {
    this.base = normalizeEndpoint(endpoint, allowInsecureHttp);
    if (!username.trim() || !password) throw new Error("WebDAV username and password are required");
    this.authHeader = `Basic ${Buffer.from(`${username}:${password}`, "utf8").toString("base64")}`;
  }

  url(...segments: string[]): string {
    return appendPath(this.base, segments);
  }

  async request(
    method: string,
    url: string,
    body?: Buffer,
    headers: Record<string, string> = {}
  ): Promise<WebDavResponse> {
    let failedAttempt = 0;
    while (true) {
      try {
        return await this.requestOnce(method, url, body, headers);
      } catch (error) {
        failedAttempt += 1;
        if (
          failedAttempt >= MAX_REQUEST_ATTEMPTS ||
          !isRetryableWebDavRequest(method, error)
        ) {
          throw error;
        }
        const retryAfterMs = error instanceof WebDavError ? error.retryAfterMs : null;
        await wait(webDavRetryDelayMs(failedAttempt, retryAfterMs));
      }
    }
  }

  private async requestOnce(
    method: string,
    url: string,
    body?: Buffer,
    headers: Record<string, string> = {}
  ): Promise<WebDavResponse> {
    const response = await net.fetch(url, {
      method,
      headers: {
        Authorization: this.authHeader,
        ...headers,
      },
      body: body ? new Uint8Array(body) : undefined,
      signal: AbortSignal.timeout(60_000),
    });
    const contentLength = Number(response.headers.get("content-length") ?? 0);
    if (contentLength > MAX_RESPONSE_BYTES) throw new Error("WebDAV response is too large");
    const data = Buffer.from(await response.arrayBuffer());
    if (data.length > MAX_RESPONSE_BYTES) throw new Error("WebDAV response is too large");
    if (response.status >= 400) {
      const message = response.status === 507
        ? "WebDAV storage quota is insufficient"
        : response.status === 401 || response.status === 403
          ? "WebDAV authentication or permission failed"
          : `WebDAV request failed (${response.status})`;
      throw new WebDavError(
        response.status,
        message,
        parseRetryAfterMs(response.headers.get("retry-after"))
      );
    }
    return {
      status: response.status,
      etag: response.headers.get("etag"),
      body: data,
    };
  }

  async get(url: string): Promise<WebDavResponse | null> {
    try {
      return await this.request("GET", url);
    } catch (error) {
      if (error instanceof WebDavError && error.status === 404) return null;
      throw error;
    }
  }

  async put(
    url: string,
    body: Buffer,
    etag: string | null,
    createOnly = false
  ): Promise<WebDavResponse> {
    const headers: Record<string, string> = {
      "Content-Type": "application/octet-stream",
    };
    if (createOnly) headers["If-None-Match"] = "*";
    else if (etag) headers["If-Match"] = etag;
    return this.request("PUT", url, body, headers);
  }

  async ensureDirectory(url: string): Promise<void> {
    try {
      await this.request("MKCOL", url);
    } catch (error) {
      if (error instanceof WebDavError && [405, 409].includes(error.status)) return;
      throw error;
    }
  }

  async ensureLayout(root: string): Promise<void> {
    const base = this.url(root, "v1");
    await this.ensureDirectory(this.url(root));
    await this.ensureDirectory(base);
    await this.ensureDirectory(this.url(root, "v1", "objects"));
    await this.ensureDirectory(this.url(root, "v1", "snapshots"));
    await this.ensureDirectory(this.url(root, "v1", "devices"));
  }

  async testConnection(root: string): Promise<{ weakEtag: boolean }> {
    const url = this.url(root);
    try {
      const response = await this.request("PROPFIND", url, Buffer.from(
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:displayname /></d:prop></d:propfind>",
        "utf8"
      ), {
        Depth: "0",
        "Content-Type": "application/xml; charset=utf-8",
      });
      await this.probeWrite(root);
      return { weakEtag: response.etag === null };
    } catch (error) {
      if (error instanceof WebDavError && error.status === 405) {
        const response = await this.request("HEAD", url);
        await this.probeWrite(root);
        return { weakEtag: response.etag === null };
      }
      throw error;
    }
  }

  private async probeWrite(root: string): Promise<void> {
    const probeUrl = this.url(root, "v1", `.connection-test-${randomUUID()}.bin`);
    await this.request("PUT", probeUrl, Buffer.from("snow-app-webdav-probe", "utf8"), {
      "Content-Type": "application/octet-stream",
      "If-None-Match": "*",
    });
    try {
      await this.request("DELETE", probeUrl);
    } catch (error) {
      if (!(error instanceof WebDavError && [404, 405].includes(error.status))) {
        throw error;
      }
    }
  }
}
