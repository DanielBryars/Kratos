import type { EpisodeData, PlaybackState, RepeatMode } from '../types';

export type PlaybackCallback = (frame: number) => void;

/**
 * Controls playback of episode frames at the dataset FPS.
 */
export class PlaybackController {
  private state: PlaybackState = {
    playing: false,
    currentFrame: 0,
    totalFrames: 0,
    speed: 1,
    fps: 30,
    repeatMode: 'repeat-one',
  };

  private lastTimestamp = 0;
  private accumulator = 0;
  private animFrameId = 0;
  private onFrame: PlaybackCallback | null = null;
  private onStateChange: ((state: PlaybackState) => void) | null = null;
  private onEpisodeEnd: (() => void) | null = null;

  setCallbacks(
    onFrame: PlaybackCallback,
    onStateChange: (state: PlaybackState) => void,
  ): void {
    this.onFrame = onFrame;
    this.onStateChange = onStateChange;
  }

  setOnEpisodeEnd(cb: () => void): void {
    this.onEpisodeEnd = cb;
  }

  setRepeatMode(mode: RepeatMode): void {
    this.state.repeatMode = mode;
    this.notify();
  }

  cycleRepeatMode(): void {
    const order: RepeatMode[] = ['off', 'repeat-all', 'repeat-one'];
    const idx = order.indexOf(this.state.repeatMode);
    this.state.repeatMode = order[(idx + 1) % order.length];
    this.notify();
  }

  loadEpisode(episode: EpisodeData): void {
    this.pause();
    this.state.totalFrames = episode.totalFrames;
    this.state.fps = episode.fps;
    this.state.currentFrame = 0;
    this.accumulator = 0;
    this.notify();
    // Show first frame
    this.onFrame?.(0);
  }

  play(): void {
    if (this.state.totalFrames === 0) return;
    this.state.playing = true;
    this.lastTimestamp = performance.now();
    this.accumulator = 0;
    this.tick(this.lastTimestamp);
    this.notify();
  }

  pause(): void {
    this.state.playing = false;
    if (this.animFrameId) {
      cancelAnimationFrame(this.animFrameId);
      this.animFrameId = 0;
    }
    this.notify();
  }

  togglePlay(): void {
    if (this.state.playing) {
      this.pause();
    } else {
      this.play();
    }
  }

  seek(frame: number): void {
    frame = Math.max(0, Math.min(frame, this.state.totalFrames - 1));
    this.state.currentFrame = frame;
    this.accumulator = 0;
    this.onFrame?.(frame);
    this.notify();
  }

  nextFrame(): void {
    if (this.state.currentFrame < this.state.totalFrames - 1) {
      this.seek(this.state.currentFrame + 1);
    }
  }

  prevFrame(): void {
    if (this.state.currentFrame > 0) {
      this.seek(this.state.currentFrame - 1);
    }
  }

  setSpeed(speed: number): void {
    this.state.speed = speed;
    this.notify();
  }

  getState(): PlaybackState {
    return { ...this.state };
  }

  private tick = (timestamp: number): void => {
    if (!this.state.playing) return;

    const dt = timestamp - this.lastTimestamp;
    this.lastTimestamp = timestamp;

    // Accumulate time, advance frames at dataset FPS * speed
    const frameDuration = 1000 / (this.state.fps * this.state.speed);
    this.accumulator += dt;

    let advanced = false;
    while (this.accumulator >= frameDuration) {
      this.accumulator -= frameDuration;
      this.state.currentFrame++;
      advanced = true;

      if (this.state.currentFrame >= this.state.totalFrames) {
        switch (this.state.repeatMode) {
          case 'repeat-one':
            this.state.currentFrame = 0;
            break;
          case 'repeat-all':
            this.state.currentFrame = this.state.totalFrames - 1;
            this.pause();
            this.onFrame?.(this.state.currentFrame);
            this.notify();
            this.onEpisodeEnd?.();
            return;
          case 'off':
            this.state.currentFrame = this.state.totalFrames - 1;
            this.pause();
            this.onFrame?.(this.state.currentFrame);
            this.notify();
            return;
        }
      }
    }

    if (advanced) {
      this.onFrame?.(this.state.currentFrame);
      this.notify();
    }

    this.animFrameId = requestAnimationFrame(this.tick);
  };

  private notify(): void {
    this.onStateChange?.({ ...this.state });
  }

  destroy(): void {
    this.pause();
    this.onFrame = null;
    this.onStateChange = null;
  }
}
