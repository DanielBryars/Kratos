/** MuJoCo WASM module type (from mujoco-js) */
export type MuJoCoModule = Awaited<ReturnType<typeof import('mujoco-js')['default']>>;

/** Dataset metadata from HF info.json */
export interface DatasetInfo {
  codebase_version: string;
  fps: number;
  total_episodes: number;
  total_frames: number;
  features: Record<string, FeatureInfo>;
  data_path: string;
  video_path: string | null;
  robot_type?: string;
  splits?: Record<string, string>;
  chunks_size?: number;
  episodes_chunk_size?: number;
}

export interface FeatureInfo {
  dtype: string;
  shape: number[];
  names?: string[] | Record<string, unknown>;
}

/** Episode metadata from v3.0 episodes parquet */
export interface EpisodeMetadata {
  episode_index: number;
  data_chunk_index: number;
  data_file_index: number;
  dataset_from_index: number;
  dataset_to_index: number;
  length: number;
}

/** A single frame of episode data */
export interface FrameData {
  /** Joint state values (6 values, typically normalized [-100,100] or radians) */
  state: number[];
  /** Optional action values */
  action?: number[];
  /** Frame index within episode */
  frameIndex: number;
  /** Timestamp */
  timestamp?: number;
}

/** Video stream info for an episode */
export interface VideoInfo {
  key: string;
  url: string;
  fromTimestamp: number;
  toTimestamp: number;
}

/** Scene object placement info */
export interface SceneObjectInfo {
  position: { x: number; y: number; z: number };
  quaternion?: { w: number; x: number; y: number; z: number };
}

/** Per-episode scene entry from episode_scenes.json */
export interface EpisodeSceneEntry {
  sceneXml?: string;
  objects: Record<string, SceneObjectInfo>;
}

/** Complete loaded episode */
export interface EpisodeData {
  frames: FrameData[];
  fps: number;
  episodeIndex: number;
  totalFrames: number;
  videos: VideoInfo[];
  sceneObjects?: Record<string, SceneObjectInfo>;
  /** Scene XML filename extracted from episode_scenes.json (just the basename, e.g. "so101_two_white_blocks.xml") */
  sceneXml?: string;
}

/** Repeat mode for episode playback */
export type RepeatMode = 'off' | 'repeat-all' | 'repeat-one';

/** Playback state */
export interface PlaybackState {
  playing: boolean;
  currentFrame: number;
  totalFrames: number;
  speed: number;
  fps: number;
  repeatMode: RepeatMode;
}
