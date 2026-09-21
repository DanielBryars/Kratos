import type { VideoInfo } from '../types';

/**
 * Manages video elements for camera feeds synced to playback.
 */
export class CameraPanel {
  private container: HTMLElement;
  private videos: Map<string, HTMLVideoElement> = new Map();
  private videoInfos: VideoInfo[] = [];
  private fps = 30;
  private totalFrames = 0;

  constructor(container: HTMLElement) {
    this.container = container;
  }

  loadVideos(infos: VideoInfo[], fps: number, totalFrames: number): void {
    // Clean up old videos
    this.videos.forEach((v) => {
      v.pause();
      v.removeAttribute('src');
      v.load();
    });
    this.container.innerHTML = '';
    this.videos.clear();

    this.videoInfos = infos;
    this.fps = fps;
    this.totalFrames = totalFrames;

    if (infos.length === 0) {
      this.container.innerHTML = '<div style="color:#666;font-size:12px;padding:8px;">No cameras</div>';
      return;
    }

    for (const info of infos) {
      const wrapper = document.createElement('div');
      wrapper.className = 'camera-wrapper';

      const label = document.createElement('div');
      label.className = 'camera-label';
      // Show a friendly name: "wrist_cam" from "observation.images.wrist_cam"
      const shortName = info.key.replace('observation.images.', '');
      label.textContent = shortName;
      wrapper.appendChild(label);

      const video = document.createElement('video');
      video.className = 'camera-video';
      video.muted = true;
      video.playsInline = true;
      video.preload = 'auto';
      video.crossOrigin = 'anonymous';
      video.src = info.url;
      video.load();
      wrapper.appendChild(video);

      this.container.appendChild(wrapper);
      this.videos.set(info.key, video);
    }
  }

  /**
   * Seek all videos to match a given frame index.
   */
  seekToFrame(frame: number): void {
    for (const info of this.videoInfos) {
      const video = this.videos.get(info.key);
      if (!video) continue;

      // Map frame to video time
      // For segmented videos: time = fromTimestamp + (frame / totalFrames) * duration
      const duration = info.toTimestamp - info.fromTimestamp;
      let time: number;
      if (duration > 0 && this.totalFrames > 0) {
        time = info.fromTimestamp + (frame / Math.max(this.totalFrames - 1, 1)) * duration;
      } else {
        // Fallback: use frame / fps
        time = frame / this.fps;
      }

      // Only seek if the difference is meaningful (avoid constant seeking jitter)
      if (Math.abs(video.currentTime - time) > 0.01) {
        video.currentTime = time;
      }
    }
  }

  clear(): void {
    this.videos.forEach((v) => {
      v.pause();
      v.removeAttribute('src');
      v.load();
    });
    this.container.innerHTML = '';
    this.videos.clear();
    this.videoInfos = [];
  }
}
