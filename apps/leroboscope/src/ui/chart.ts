import type { EpisodeData } from '../types';
import type { UnitMode } from '../replay/joint-mapper';
import { MOTOR_NAMES } from '../constants';

const COLORS = [
  '#4fc3f7', // shoulder_pan - light blue
  '#81c784', // shoulder_lift - green
  '#ffb74d', // elbow_flex - orange
  '#e57373', // wrist_flex - red
  '#ba68c8', // wrist_roll - purple
  '#fff176', // gripper - yellow
];

/**
 * Manages a canvas-based joint position chart with a playhead.
 */
export class JointChart {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private episode: EpisodeData | null = null;
  private currentFrame = 0;
  private resizeObserver: ResizeObserver;
  private unitMode: UnitMode = 'normalized';
  private dataSource: 'action' | 'state' = 'action';

  constructor(container: HTMLElement) {
    this.canvas = document.createElement('canvas');
    this.canvas.style.width = '100%';
    this.canvas.style.height = '100%';
    this.canvas.style.display = 'block';
    container.appendChild(this.canvas);
    this.ctx = this.canvas.getContext('2d')!;

    this.resizeObserver = new ResizeObserver(() => this.draw());
    this.resizeObserver.observe(container);

    // Click to seek
    this.canvas.addEventListener('click', (e) => {
      if (!this.episode || !this.onSeek) return;
      const rect = this.canvas.getBoundingClientRect();
      const x = e.clientX - rect.left;
      const plotLeft = 50;
      const plotRight = this.canvas.width / devicePixelRatio - 10;
      const plotWidth = plotRight - plotLeft;
      if (x < plotLeft || x > plotRight) return;
      const fraction = (x - plotLeft) / plotWidth;
      const frame = Math.round(fraction * (this.episode.totalFrames - 1));
      this.onSeek(Math.max(0, Math.min(frame, this.episode.totalFrames - 1)));
    });
  }

  onSeek: ((frame: number) => void) | null = null;

  loadEpisode(episode: EpisodeData): void {
    this.episode = episode;
    this.currentFrame = 0;
    this.draw();
  }

  setFrame(frame: number): void {
    this.currentFrame = frame;
    this.draw();
  }

  setUnitMode(mode: UnitMode): void {
    this.unitMode = mode;
    this.draw();
  }

  setDataSource(source: 'action' | 'state'): void {
    this.dataSource = source;
    this.draw();
  }

  private draw(): void {
    const dpr = devicePixelRatio || 1;
    const rect = this.canvas.parentElement!.getBoundingClientRect();
    const w = rect.width;
    const h = rect.height;

    this.canvas.width = w * dpr;
    this.canvas.height = h * dpr;
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    const ctx = this.ctx;
    ctx.fillStyle = '#111';
    ctx.fillRect(0, 0, w, h);

    if (!this.episode || this.episode.totalFrames === 0) {
      ctx.fillStyle = '#666';
      ctx.font = '12px sans-serif';
      ctx.textAlign = 'center';
      ctx.fillText('No data loaded', w / 2, h / 2);
      return;
    }

    const ep = this.episode;
    const frames = ep.frames;
    const numFrames = frames.length;

    // Helper to get the values array for a frame based on data source selection
    const getValues = (frame: typeof frames[0]): number[] => {
      if (this.dataSource === 'action' && frame.action) return frame.action;
      return frame.state;
    };

    // Layout
    const plotLeft = 50;
    const plotRight = w - 10;
    const plotTop = 8;
    const plotBottom = h - 20;
    const plotWidth = plotRight - plotLeft;
    const plotHeight = plotBottom - plotTop;

    // Find Y range across all joints
    let yMin: number;
    let yMax: number;
    if (this.unitMode === 'normalized') {
      // Fixed axis for normalized mode
      yMin = -110;
      yMax = 110;
    } else {
      yMin = Infinity;
      yMax = -Infinity;
      for (const frame of frames) {
        for (const v of getValues(frame)) {
          if (v < yMin) yMin = v;
          if (v > yMax) yMax = v;
        }
      }
      // Add padding
      const yPad = (yMax - yMin) * 0.05 || 1;
      yMin -= yPad;
      yMax += yPad;
    }

    // Draw grid lines
    ctx.strokeStyle = '#2a2a2a';
    ctx.lineWidth = 0.5;
    const yTicks = 5;
    for (let i = 0; i <= yTicks; i++) {
      const y = plotTop + (i / yTicks) * plotHeight;
      ctx.beginPath();
      ctx.moveTo(plotLeft, y);
      ctx.lineTo(plotRight, y);
      ctx.stroke();

      // Y axis labels
      const val = yMax - (i / yTicks) * (yMax - yMin);
      ctx.fillStyle = '#666';
      ctx.font = '10px sans-serif';
      ctx.textAlign = 'right';
      ctx.fillText(val.toFixed(0), plotLeft - 4, y + 3);
    }

    // Zero line
    if (yMin < 0 && yMax > 0) {
      const zeroY = plotTop + ((yMax - 0) / (yMax - yMin)) * plotHeight;
      ctx.strokeStyle = '#444';
      ctx.lineWidth = 1;
      ctx.setLineDash([4, 4]);
      ctx.beginPath();
      ctx.moveTo(plotLeft, zeroY);
      ctx.lineTo(plotRight, zeroY);
      ctx.stroke();
      ctx.setLineDash([]);
    }

    // Draw joint lines
    const numJoints = Math.min(getValues(frames[0]).length, 6);
    for (let j = 0; j < numJoints; j++) {
      ctx.strokeStyle = COLORS[j];
      ctx.lineWidth = 1.5;
      ctx.beginPath();

      for (let i = 0; i < numFrames; i++) {
        const x = plotLeft + (i / (numFrames - 1)) * plotWidth;
        const v = getValues(frames[i])[j];
        const y = plotTop + ((yMax - v) / (yMax - yMin)) * plotHeight;

        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
      ctx.stroke();
    }

    // Playhead
    const phX = plotLeft + (this.currentFrame / Math.max(numFrames - 1, 1)) * plotWidth;
    ctx.strokeStyle = '#fff';
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.moveTo(phX, plotTop);
    ctx.lineTo(phX, plotBottom);
    ctx.stroke();

    // Legend
    const legendX = plotLeft + 4;
    const legendY = plotTop + 4;
    ctx.font = '10px sans-serif';
    ctx.textAlign = 'left';
    for (let j = 0; j < numJoints; j++) {
      const lx = legendX + (j % 3) * 100;
      const ly = legendY + Math.floor(j / 3) * 14 + 10;
      ctx.fillStyle = COLORS[j];
      ctx.fillRect(lx, ly - 6, 8, 3);
      ctx.fillText(MOTOR_NAMES[j] ?? `joint_${j}`, lx + 12, ly);
    }

    // X axis labels: seconds and frames
    const fps = ep.fps || 30;
    const totalSec = (numFrames - 1) / fps;
    const curSec = this.currentFrame / fps;
    ctx.fillStyle = '#666';
    ctx.font = '10px sans-serif';
    ctx.textAlign = 'left';
    ctx.fillText('0s / f0', plotLeft, plotBottom + 14);
    ctx.textAlign = 'right';
    ctx.fillText(`${totalSec.toFixed(1)}s / f${numFrames - 1}`, plotRight, plotBottom + 14);

    // Playhead time label
    ctx.fillStyle = '#ccc';
    ctx.textAlign = 'center';
    ctx.fillText(`${curSec.toFixed(2)}s / f${this.currentFrame}`, phX, plotBottom + 14);
  }
}
