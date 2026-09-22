import {
  detectVersion,
  fetchDatasetInfo,
  fetchParquetFile,
  formatDataPath,
  huggingFaceSource,
  kratosUploadSource,
  type DatasetFileSource,
} from './hf-client';
import { readParquetAsObjects } from './parquet-reader';
import type { DatasetInfo, EpisodeMetadata, EpisodeData, FrameData, VideoInfo, EpisodeSceneEntry } from '../types';

const HF_BASE = 'https://huggingface.co';

export interface DatasetContext {
  repoId: string;
  version: string;
  revision: string;
  source: DatasetFileSource;
  info: DatasetInfo;
  episodeScenes: Record<string, EpisodeSceneEntry> | null;
}

export async function loadDatasetContext(repoId: string, requestedRevision?: string): Promise<DatasetContext> {
  const { version, revision } = await detectVersion(repoId, requestedRevision);
  const info = await fetchDatasetInfo(repoId, revision);
  const source = huggingFaceSource(repoId, revision);
  const episodeScenes = await fetchEpisodeScenes(source);
  return { repoId, version, revision, source, info, episodeScenes };
}

export async function loadUploadedDatasetContext(
  label: string,
  revision: string,
  info: DatasetInfo,
  fileBaseUrl: string,
): Promise<DatasetContext> {
  const rawVersion = info.codebase_version || '2.0';
  const version = rawVersion.startsWith('v') ? rawVersion : `v${rawVersion}`;
  const source = kratosUploadSource(label, revision, fileBaseUrl);
  const episodeScenes = await fetchEpisodeScenes(source);
  return { repoId: label, version, revision, source, info, episodeScenes };
}

/**
 * Extract just the filename from a scene_xml path (which may be an absolute local path).
 * e.g. "E:\git\ai\lerobot-thesis\scenes\so101_two_white_blocks.xml" → "so101_two_white_blocks.xml"
 */
function sceneXmlBasename(path: string): string {
  // Handle both forward and backslashes
  const parts = path.replace(/\\/g, '/').split('/');
  return parts[parts.length - 1];
}

/**
 * Try to fetch meta/episode_scenes.json (contains scene_xml + object positions per episode).
 */
async function fetchEpisodeScenes(
  source: DatasetFileSource,
): Promise<Record<string, EpisodeSceneEntry> | null> {
  try {
    const url = source.url('meta/episode_scenes.json');
    const res = await fetch(url);
    if (!res.ok) return null;
    const raw = await res.json();
    // Format: { "0": { "scene_xml": "path/to/scene.xml", "objects": { "duplo": { position: ..., quaternion: ... } } } }
    const result: Record<string, EpisodeSceneEntry> = {};
    for (const [epIdx, epData] of Object.entries(raw)) {
      const entry = epData as any;
      const objects = entry?.objects ?? {};
      const sceneXmlRaw = entry?.scene_xml;
      result[epIdx] = {
        objects,
        sceneXml: sceneXmlRaw ? sceneXmlBasename(sceneXmlRaw) : undefined,
      };
    }
    const withScene = Object.values(result).filter(e => e.sceneXml).length;
    console.log(`[episode_scenes] Loaded scene info for ${Object.keys(result).length} episodes (${withScene} with scene_xml)`);
    return result;
  } catch {
    console.log('[episode_scenes] No episode_scenes.json found (optional)');
    return null;
  }
}

export async function loadEpisode(
  ctx: DatasetContext,
  episodeIndex: number,
): Promise<EpisodeData> {
  if (ctx.version === 'v3.0') {
    return loadEpisodeV3(ctx, episodeIndex);
  }
  return loadEpisodeV2(ctx, episodeIndex);
}

async function loadEpisodeV3(
  ctx: DatasetContext,
  episodeIndex: number,
): Promise<EpisodeData> {
  const epMeta = await loadEpisodeMetadata(ctx, episodeIndex);
  const epMetaRaw = await loadEpisodeMetadataRaw(ctx, episodeIndex);

  const chunk = epMeta.data_chunk_index;
  const file = epMeta.data_file_index;
  const dataPath = `data/chunk-${pad3(chunk)}/file-${pad3(file)}.parquet`;

  const buffer = await fetchParquetFile(ctx.source, dataPath);
  const fullData = await readParquetAsObjects(buffer);

  const fromIndex = epMeta.dataset_from_index;
  const toIndex = epMeta.dataset_to_index;

  let fileStartIndex = 0;
  if (fullData.length > 0 && fullData[0].index !== undefined) {
    fileStartIndex = Number(fullData[0].index);
  }
  const localFrom = Math.max(0, fromIndex - fileStartIndex);
  const localTo = Math.min(fullData.length, toIndex - fileStartIndex);
  const episodeRows = fullData.slice(localFrom, localTo);

  const frames = extractFrames(episodeRows);
  const videos = extractVideoInfoV3(ctx, epMetaRaw);
  const sceneEntry = ctx.episodeScenes?.[episodeIndex.toString()];
  const sceneObjects = sceneEntry?.objects;

  return {
    frames,
    fps: ctx.info.fps,
    episodeIndex,
    totalFrames: frames.length,
    videos,
    sceneObjects: sceneObjects && Object.keys(sceneObjects).length > 0 ? sceneObjects : undefined,
    sceneXml: sceneEntry?.sceneXml,
  };
}

async function loadEpisodeV2(
  ctx: DatasetContext,
  episodeIndex: number,
): Promise<EpisodeData> {
  const episodeChunk = Math.floor(episodeIndex / (ctx.info.chunks_size || 1000));
  const dataPath = formatDataPath(ctx.info.data_path, {
    episode_chunk: episodeChunk,
    episode_index: episodeIndex,
  });

  const buffer = await fetchParquetFile(ctx.source, dataPath);
  const rows = await readParquetAsObjects(buffer);

  const frames = extractFrames(rows);
  // v2 video paths use a simpler template
  const videos = extractVideoInfoV2(ctx, episodeIndex, episodeChunk);
  const sceneEntry = ctx.episodeScenes?.[episodeIndex.toString()];
  const sceneObjects = sceneEntry?.objects;

  return {
    frames,
    fps: ctx.info.fps,
    episodeIndex,
    totalFrames: frames.length,
    videos,
    sceneObjects: sceneObjects && Object.keys(sceneObjects).length > 0 ? sceneObjects : undefined,
    sceneXml: sceneEntry?.sceneXml,
  };
}

// --- Episode metadata ---

async function loadEpisodeMetadata(
  ctx: DatasetContext,
  episodeIndex: number,
): Promise<EpisodeMetadata> {
  const row = await loadEpisodeMetadataRaw(ctx, episodeIndex);
  return parseEpisodeMetadata(row);
}

async function loadEpisodeMetadataRaw(
  ctx: DatasetContext,
  episodeIndex: number,
): Promise<Record<string, unknown>> {
  let fileIdx = 0;
  const chunkIdx = 0;

  while (true) {
    const path = `meta/episodes/chunk-${pad3(chunkIdx)}/file-${pad3(fileIdx)}.parquet`;
    try {
      const buffer = await fetchParquetFile(ctx.source, path);
      const rows = await readParquetAsObjects(buffer);

      for (const row of rows) {
        const epIdx = toNumber(row.episode_index ?? row['0']);
        if (epIdx === episodeIndex) return row;
      }
      fileIdx++;
    } catch {
      throw new Error(`Episode ${episodeIndex} not found in metadata`);
    }
  }
}

function parseEpisodeMetadata(row: Record<string, unknown>): EpisodeMetadata {
  if ('episode_index' in row) {
    return {
      episode_index: toNumber(row.episode_index),
      data_chunk_index: toNumber(row['data/chunk_index'] ?? row.data_chunk_index ?? 0),
      data_file_index: toNumber(row['data/file_index'] ?? row.data_file_index ?? 0),
      dataset_from_index: toNumber(row.dataset_from_index ?? 0),
      dataset_to_index: toNumber(row.dataset_to_index ?? 0),
      length: toNumber(row.length ?? 0),
    };
  }
  return {
    episode_index: toNumber(row['0'] ?? 0),
    data_chunk_index: toNumber(row['1'] ?? 0),
    data_file_index: toNumber(row['2'] ?? 0),
    dataset_from_index: toNumber(row['3'] ?? 0),
    dataset_to_index: toNumber(row['4'] ?? 0),
    length: toNumber(row['9'] ?? 0),
  };
}

// --- Video info extraction ---

function extractVideoInfoV3(
  ctx: DatasetContext,
  epRow: Record<string, unknown>,
): VideoInfo[] {
  const videoFeatures = Object.entries(ctx.info.features).filter(
    ([, v]) => v.dtype === 'video',
  );

  return videoFeatures.map(([videoKey]) => {
    // Look for per-camera metadata: videos/{key}/chunk_index, file_index, from_timestamp, to_timestamp
    const chunkVal = epRow[`videos/${videoKey}/chunk_index`];
    const fileVal = epRow[`videos/${videoKey}/file_index`];
    const fromTs = epRow[`videos/${videoKey}/from_timestamp`];
    const toTs = epRow[`videos/${videoKey}/to_timestamp`];

    const chunkIndex = chunkVal !== undefined ? toNumber(chunkVal) : 0;
    const fileIndex = fileVal !== undefined ? toNumber(fileVal) : 0;
    const fromTimestamp = fromTs !== undefined ? toNumber(fromTs) : 0;
    const toTimestamp = toTs !== undefined ? toNumber(toTs) : 30;

    const videoPath = `videos/${videoKey}/chunk-${pad3(chunkIndex)}/file-${pad3(fileIndex)}.mp4`;
    const url = ctx.source.url(videoPath);

    return { key: videoKey, url, fromTimestamp, toTimestamp };
  });
}

function extractVideoInfoV2(
  ctx: DatasetContext,
  episodeIndex: number,
  episodeChunk: number,
): VideoInfo[] {
  if (!ctx.info.video_path) return [];

  const videoFeatures = Object.entries(ctx.info.features).filter(
    ([, v]) => v.dtype === 'video',
  );

  return videoFeatures.map(([videoKey]) => {
    const videoPath = ctx.info.video_path!
      .replace(/{video_key}/g, videoKey)
      .replace(/{episode_chunk(?::\d+d)?}/g, pad3(episodeChunk))
      .replace(/{episode_index(?::\d+d)?}/g, episodeIndex.toString().padStart(6, '0'));

    const url = ctx.source.url(videoPath);
    return { key: videoKey, url, fromTimestamp: 0, toTimestamp: 0 };
  });
}

// --- Frame extraction ---

/**
 * Try to extract a flat number[] from a parquet value that might be:
 * - A plain JS array: [1.0, 2.0, ...]
 * - A typed array: Float32Array, Float64Array, etc.
 * - A nested Parquet list: [{list: [{element: v}]}, ...] or [[v], [v], ...]
 */
function toNumberArray(val: unknown): number[] | null {
  if (val == null) return null;

  // Plain JS array
  if (Array.isArray(val)) {
    if (val.length === 0) return null;
    // Check if first element is a number (flat array)
    if (typeof val[0] === 'number' || typeof val[0] === 'bigint') {
      return val.map(Number);
    }
    // Nested: [[v], [v], ...] or [{element: v}, ...]
    if (Array.isArray(val[0])) {
      return val.map((sub: unknown) => Number((sub as unknown[])[0]));
    }
    if (typeof val[0] === 'object' && val[0] !== null) {
      const obj = val[0] as Record<string, unknown>;
      const key = Object.keys(obj)[0];
      if (key) return val.map((sub: unknown) => Number((sub as Record<string, unknown>)[key]));
    }
    return val.map(Number);
  }

  // Typed arrays (Float32Array, Float64Array, etc.)
  if (ArrayBuffer.isView(val) && 'length' in val) {
    return Array.from(val as Float32Array);
  }

  return null;
}

function extractFrames(rows: Record<string, unknown>[]): FrameData[] {
  const frames: FrameData[] = [];

  // Log first row structure for debugging
  if (rows.length > 0) {
    const row0 = rows[0];
    const keys = Object.keys(row0);
    console.log(`[extractFrames] ${rows.length} rows, columns (${keys.length}):`, keys);
    for (const key of keys) {
      const val = row0[key];
      let preview: string;
      if (Array.isArray(val) || ArrayBuffer.isView(val)) {
        const arr = Array.isArray(val) ? val : Array.from(val as Float32Array);
        preview = `[${arr.slice(0, 3).map(Number).join(', ')}...] (len=${arr.length})`;
      } else if (typeof val === 'bigint') {
        preview = `${val}n`;
      } else {
        preview = String(val);
      }
      console.log(`  "${key}": ${preview}`);
    }
  }

  for (let i = 0; i < rows.length; i++) {
    const row = rows[i];
    let state: number[] | null = null;
    let action: number[] | undefined;

    state = toNumberArray(row['observation.state']);
    const actionArr = toNumberArray(row['action']);
    if (actionArr) action = actionArr;

    if (!state && action) {
      state = action;
    }

    // Fallback: find first array-like value with expected joint count
    if (!state) {
      for (const [key, val] of Object.entries(row)) {
        const arr = toNumberArray(val);
        if (arr && arr.length >= 4 && arr.length <= 12) {
          console.log(`[extractFrames] Fallback: using column "${key}" (${arr.length} elements)`);
          state = arr;
          break;
        }
      }
    }

    if (state) {
      frames.push({
        state,
        action,
        frameIndex: i,
        timestamp: row.timestamp !== undefined ? Number(row.timestamp) : undefined,
      });
    }
  }

  if (frames.length > 0) {
    console.log('[extractFrames] Frame 0 state:', frames[0].state);
    console.log('[extractFrames] Frame 0 action:', frames[0].action);
  }

  return frames;
}

// --- Helpers ---

function toNumber(val: unknown): number {
  if (typeof val === 'bigint') return Number(val);
  if (typeof val === 'number') return val;
  return Number(val) || 0;
}

function pad3(n: number): string {
  return n.toString().padStart(3, '0');
}
