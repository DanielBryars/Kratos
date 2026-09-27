import type { DatasetInfo } from '../types';

const HF_BASE = 'https://huggingface.co';

export interface DatasetFileSource {
  readonly label: string;
  readonly revision: string;
  resolve(path: string): Promise<string>;
}

export function huggingFaceSource(repoId: string, revision: string): DatasetFileSource {
  return {
    label: repoId,
    revision,
    resolve: async (path: string) => resolveUrl(repoId, revision, path),
  };
}

export function kratosUploadSource(
  label: string,
  revision: string,
  fileBaseUrl: string,
  previewToken: string,
): DatasetFileSource {
  const base = fileBaseUrl.replace(/\/$/, '');
  return {
    label,
    revision,
    resolve: async (path: string) => {
      const endpoint = `${base}/${path.split('/').map(encodeURIComponent).join('/')}`;
      const response = await fetch(endpoint, {
        headers: {
          Accept: 'application/json',
          Authorization: `Bearer ${previewToken}`,
        },
      });
      if (!response.ok) {
        throw new Error(`Kratos could not open ${path}: ${response.status} ${response.statusText}`);
      }
      const body = await response.json() as { url?: unknown };
      if (typeof body.url !== 'string' || !body.url.startsWith('https://')) {
        throw new Error(`Kratos returned no signed read for ${path}`);
      }
      return body.url;
    },
  };
}

/**
 * Build a resolve URL for a file in a HF dataset repo.
 */
function resolveUrl(repoId: string, revision: string, path: string): string {
  return `${HF_BASE}/datasets/${repoId}/resolve/${revision}/${path}`;
}

/**
 * Detect dataset version by checking which refs/convert tags exist.
 * Returns "v3.0", "v2.1", or "v2.0".
 */
export async function detectVersion(
  repoId: string,
  requestedRevision?: string,
): Promise<{ version: string; revision: string }> {
  if (requestedRevision) {
    const info = await fetchDatasetInfo(repoId, requestedRevision);
    const rawVersion = info.codebase_version || '2.0';
    const version = rawVersion.startsWith('v') ? rawVersion : `v${rawVersion}`;
    return { version, revision: requestedRevision };
  }
  // Try v3.0 first (main branch)
  try {
    const url = resolveUrl(repoId, 'main', 'meta/info.json');
    const res = await fetch(url);
    if (res.ok) {
      const info = await res.json() as DatasetInfo;
      if (info.codebase_version === '3.0' || info.codebase_version?.startsWith('v3')) {
        return { version: 'v3.0', revision: 'main' };
      }
      // v2.x on main
      const ver = info.codebase_version?.startsWith('v') ? info.codebase_version : `v${info.codebase_version}`;
      return { version: ver, revision: 'main' };
    }
  } catch { /* try next */ }

  // Fallback: check refs/convert/parquet
  try {
    const url = resolveUrl(repoId, 'refs/convert/parquet', 'meta/info.json');
    const res = await fetch(url);
    if (res.ok) {
      return { version: 'v2.0', revision: 'refs/convert/parquet' };
    }
  } catch { /* not found */ }

  throw new Error(`Could not detect dataset version for ${repoId}`);
}

/**
 * Fetch dataset info.json metadata.
 */
export async function fetchDatasetInfo(repoId: string, revision: string): Promise<DatasetInfo> {
  const url = resolveUrl(repoId, revision, 'meta/info.json');
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`Failed to fetch info.json: ${res.status} ${res.statusText}`);
  }
  return res.json() as Promise<DatasetInfo>;
}

/**
 * Fetch a parquet file as ArrayBuffer.
 */
export async function fetchParquetFile(source: DatasetFileSource, path: string): Promise<ArrayBuffer> {
  const url = await source.resolve(path);
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`Failed to fetch ${path}: ${res.status} ${res.statusText}`);
  }
  return res.arrayBuffer();
}

/**
 * Build data path from template and variables.
 * Handles format strings like "data/chunk-{episode_chunk:03d}/episode_{episode_index:06d}.parquet"
 */
export function formatDataPath(template: string, vars: Record<string, number>): string {
  return template.replace(/{(\w+)(?::(\d+)d)?}/g, (_match, key, pad) => {
    const val = vars[key];
    if (val === undefined) return _match;
    if (pad) {
      return val.toString().padStart(parseInt(pad), '0');
    }
    return val.toString();
  });
}
