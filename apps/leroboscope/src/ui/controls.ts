import type { PlaybackState, RepeatMode } from '../types';
import type { EpisodeDecision } from '../kratos/review-client';

/** All the UI elements we need */
export interface UIElements {
  sceneSelect: HTMLSelectElement;
  datasetInput: HTMLInputElement;
  episodeList: HTMLElement;
  loadBtn: HTMLButtonElement;
  playPauseBtn: HTMLButtonElement;
  prevFrameBtn: HTMLButtonElement;
  nextFrameBtn: HTMLButtonElement;
  timeline: HTMLInputElement;
  frameInfo: HTMLSpanElement;
  speedSelect: HTMLSelectElement;
  dataSourceSelect: HTMLSelectElement;
  unitsSelect: HTMLSelectElement;
  repeatBtn: HTMLButtonElement;
  datasetMeta: HTMLSpanElement;
}

export function getUIElements(): UIElements {
  return {
    sceneSelect: document.getElementById('scene-select') as HTMLSelectElement,
    datasetInput: document.getElementById('dataset-id') as HTMLInputElement,
    episodeList: document.getElementById('episode-list') as HTMLElement,
    loadBtn: document.getElementById('load-btn') as HTMLButtonElement,
    playPauseBtn: document.getElementById('play-pause') as HTMLButtonElement,
    prevFrameBtn: document.getElementById('prev-frame') as HTMLButtonElement,
    nextFrameBtn: document.getElementById('next-frame') as HTMLButtonElement,
    timeline: document.getElementById('timeline') as HTMLInputElement,
    frameInfo: document.getElementById('frame-info') as HTMLSpanElement,
    speedSelect: document.getElementById('speed-select') as HTMLSelectElement,
    dataSourceSelect: document.getElementById('data-source-select') as HTMLSelectElement,
    unitsSelect: document.getElementById('units-select') as HTMLSelectElement,
    repeatBtn: document.getElementById('repeat-btn') as HTMLButtonElement,
    datasetMeta: document.getElementById('dataset-meta') as HTMLSpanElement,
  };
}

/**
 * Populate episode list sidebar.
 */
export function populateEpisodes(
  ui: UIElements,
  totalEpisodes: number,
  onSelect: (index: number) => void,
): void {
  ui.episodeList.innerHTML = '';
  for (let i = 0; i < totalEpisodes; i++) {
    const item = document.createElement('div');
    item.className = 'episode-item';
    item.dataset.episode = i.toString();
    item.textContent = `Episode ${i}`;
    item.addEventListener('click', () => onSelect(i));
    ui.episodeList.appendChild(item);
  }
}

/**
 * Highlight the selected episode in the list.
 */
export function selectEpisodeInList(ui: UIElements, index: number): void {
  // Remove previous selection
  const prev = ui.episodeList.querySelector('.episode-item.active');
  if (prev) prev.classList.remove('active');

  // Select new
  const item = ui.episodeList.querySelector(`[data-episode="${index}"]`);
  if (item) {
    item.classList.add('active');
    // Scroll into view if needed
    item.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  }
}

export function setEpisodeDecision(ui: UIElements, index: number, decision: EpisodeDecision): void {
  const item = ui.episodeList.querySelector(`[data-episode="${index}"]`);
  if (!item) return;
  item.setAttribute('data-decision', decision);
  const label = decision === 'included' ? 'Included' : decision === 'excluded' ? 'Excluded' : 'Review';
  item.setAttribute('title', `Episode ${index}: ${label}`);
}

/**
 * Update the playback controls to reflect current state.
 */
export function updatePlaybackUI(ui: UIElements, state: PlaybackState): void {
  // Play/pause button
  ui.playPauseBtn.innerHTML = state.playing ? '&#9646;&#9646;' : '&#9654;';
  ui.playPauseBtn.title = state.playing ? 'Pause [Space]' : 'Play [Space]';

  // Timeline
  ui.timeline.max = Math.max(0, state.totalFrames - 1).toString();
  ui.timeline.value = state.currentFrame.toString();

  // Frame info
  ui.frameInfo.textContent = `${state.currentFrame + 1} / ${state.totalFrames}`;
}

/**
 * Set dataset metadata display.
 */
export function setDatasetMeta(ui: UIElements, fps: number, totalEpisodes: number, totalFrames: number): void {
  ui.datasetMeta.textContent = `${fps} fps | ${totalEpisodes} ep | ${totalFrames} frames`;
}

/**
 * Initialize URL parameter reading and apply to UI.
 */
export function applyUrlParams(ui: UIElements): { dataset?: string; revision?: string; episode?: number } {
  const params = new URLSearchParams(window.location.search);
  const dataset = params.get('dataset') || undefined;
  const revision = params.get('revision') || undefined;
  const episodeStr = params.get('episode');
  const episode = episodeStr ? parseInt(episodeStr, 10) : undefined;

  if (dataset) {
    ui.datasetInput.value = dataset;
  }

  return { dataset, revision, episode };
}

/**
 * Update URL parameters without reloading the page.
 */
export function updateUrlParams(dataset: string, revision: string, episode: number): void {
  const url = new URL(window.location.href);
  url.searchParams.set('dataset', dataset);
  url.searchParams.set('revision', revision);
  url.searchParams.set('episode', episode.toString());
  window.history.replaceState({}, '', url.toString());
}

const REPEAT_SVG_OFF = `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="17 1 21 5 17 9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7 23 3 19 7 15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/></svg>`;

const REPEAT_SVG_ALL = `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="17 1 21 5 17 9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7 23 3 19 7 15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/></svg>`;

const REPEAT_SVG_ONE = `<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="17 1 21 5 17 9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7 23 3 19 7 15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/><text x="12" y="14" text-anchor="middle" font-size="8" font-weight="bold" fill="currentColor" stroke="none">1</text></svg>`;

/**
 * Update the repeat button appearance based on the current mode.
 */
export function updateRepeatButton(ui: UIElements, mode: RepeatMode): void {
  switch (mode) {
    case 'off':
      ui.repeatBtn.innerHTML = REPEAT_SVG_OFF;
      ui.repeatBtn.classList.remove('active');
      ui.repeatBtn.title = 'Repeat: Off';
      break;
    case 'repeat-all':
      ui.repeatBtn.innerHTML = REPEAT_SVG_ALL;
      ui.repeatBtn.classList.add('active');
      ui.repeatBtn.title = 'Repeat: All';
      break;
    case 'repeat-one':
      ui.repeatBtn.innerHTML = REPEAT_SVG_ONE;
      ui.repeatBtn.classList.add('active');
      ui.repeatBtn.title = 'Repeat: One';
      break;
  }
}
