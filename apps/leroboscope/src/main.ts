import { initMuJoCo } from './mujoco/loader';
import { buildScene, updateBodyTransforms } from './mujoco/scene-builder';
import { createRenderer } from './mujoco/renderer';
import {
  loadDatasetContext,
  loadEpisode,
  loadUploadedDatasetContext,
  type DatasetContext,
} from './dataset/episode-loader';
import { convertToRadians, getJointQposIndices, setJointPositions, type UnitMode } from './replay/joint-mapper';
import { PlaybackController } from './replay/playback-controller';
import {
  getUIElements,
  populateEpisodes,
  selectEpisodeInList,
  updatePlaybackUI,
  updateRepeatButton,
  setEpisodeDecision,
  setDatasetMeta,
  applyUrlParams,
  updateUrlParams,
} from './ui/controls';
import { showLoading, showError, hideStatus } from './ui/status';
import { JointChart } from './ui/chart';
import { CameraPanel } from './ui/cameras';
import { DEFAULT_DATASET, DEFAULT_SCENE, SCENES } from './constants';

/** Currently loaded scene filename (e.g. "so101_with_wrist_cam.xml") */
let currentSceneFile = DEFAULT_SCENE;
import { FovOverlay } from './mujoco/fov-overlay';
import type { EpisodeData } from './types';
import {
  listenForReviewContext,
  saveEpisodeDecision,
  type EpisodeDecision,
  type ReviewContext,
} from './kratos/review-client';

// --- Global State ---
let mujoco: any;
let model: any;
let data: any;
let qposIndices: number[] = [];
let freeJointQposAddrs: Map<string, number> = new Map();
let currentEpisode: EpisodeData | null = null;
let datasetCtx: DatasetContext | null = null;
let bodyGroups: Map<number, import('three').Group> = new Map();
let sceneRoot: import('three').Group | null = null;
let fovOverlay: FovOverlay | null = null;
let reviewContext: ReviewContext | null = null;

const playback = new PlaybackController();

/** Track the current episode index for repeat-all auto-advance */
let currentEpisodeIndex = 0;

async function main() {
  const ui = getUIElements();
  const { dataset: urlDataset, revision: urlRevision, episode: urlEpisode } = applyUrlParams(ui);

  // --- Landing page elements ---
  const landingPage = document.getElementById('landing-page')!;
  const landingLoading = document.getElementById('landing-loading')!;
  const landingEnter = document.getElementById('landing-enter') as HTMLButtonElement;
  const aboutBtn = document.getElementById('about-btn') as HTMLButtonElement;
  const hasDatasetParam = !!urlDataset;
  const curationPanel = document.getElementById('curation-panel')!;
  const curationState = document.getElementById('curation-state')!;
  const curationNote = document.getElementById('curation-note') as HTMLInputElement;

  listenForReviewContext((context) => {
    reviewContext = context;
    curationPanel.removeAttribute('hidden');
    for (const [episode, decision] of Object.entries(context.decisions ?? {})) {
      setEpisodeDecision(ui, Number(episode), decision);
    }
    const current = context.decisions?.[currentEpisodeIndex.toString()];
    curationState.textContent = current ? current.replace('_', ' ') : 'Not reviewed';
    if (context.source?.kind === 'uploaded') {
      void loadUploadedDataset(context.source);
    }
  });

  async function curate(decision: EpisodeDecision) {
    if (!reviewContext) return;
    const buttons = Array.from(curationPanel.querySelectorAll('button')) as HTMLButtonElement[];
    buttons.forEach((button) => { button.disabled = true; });
    curationState.textContent = 'Saving…';
    try {
      await saveEpisodeDecision(reviewContext, currentEpisodeIndex, decision, curationNote.value);
      reviewContext.decisions = { ...reviewContext.decisions, [currentEpisodeIndex]: decision };
      setEpisodeDecision(ui, currentEpisodeIndex, decision);
      curationState.textContent = decision.replace('_', ' ');
    } catch (error) {
      curationState.textContent = `Could not save: ${error}`;
    } finally {
      buttons.forEach((button) => { button.disabled = false; });
    }
  }

  curationPanel.querySelector('[data-decision="included"]')?.addEventListener('click', () => void curate('included'));
  curationPanel.querySelector('[data-decision="excluded"]')?.addEventListener('click', () => void curate('excluded'));
  curationPanel.querySelector('[data-decision="needs_review"]')?.addEventListener('click', () => void curate('needs_review'));

  function showLandingReady() {
    landingLoading.style.display = 'none';
    landingEnter.style.display = 'inline-block';
  }

  function dismissLanding() {
    landingPage.classList.add('hidden');
    setTimeout(() => landingPage.classList.add('removed'), 500);
  }

  function showLanding() {
    landingPage.classList.remove('removed');
    // Force reflow so transition works
    void landingPage.offsetHeight;
    landingPage.classList.remove('hidden');
    landingLoading.style.display = 'none';
    landingEnter.style.display = 'inline-block';
  }

  landingEnter.addEventListener('click', dismissLanding);
  aboutBtn.addEventListener('click', showLanding);

  if (!urlDataset) {
    ui.datasetInput.value = DEFAULT_DATASET;
  }

  // Populate scene dropdown
  for (const s of SCENES) {
    const opt = document.createElement('option');
    opt.value = s.file;
    opt.textContent = s.label;
    ui.sceneSelect.appendChild(opt);
  }
  ui.sceneSelect.value = DEFAULT_SCENE;

  // --- Init chart + cameras ---
  const chart = new JointChart(document.getElementById('chart-panel')!);
  const cameras = new CameraPanel(document.getElementById('camera-container')!);

  // Chart click-to-seek
  chart.onSeek = (frame) => playback.seek(frame);

  // --- 1. Initialize Three.js (once) ---
  const viewport = document.getElementById('viewport')!;
  const { scene, camera, renderer, controls } = createRenderer(viewport);

  // --- 2. Load MuJoCo scene (can be called again on scene switch) ---
  async function loadScene(sceneFile: string) {
    showLoading('Initializing MuJoCo WASM...');

    try {
      const result = await initMuJoCo(sceneFile);
      mujoco = result.mujoco;
      model = result.model;
      data = result.data;
      currentSceneFile = sceneFile;
    } catch (err) {
      showError(`Failed to initialize MuJoCo: ${err}`);
      console.error(err);
      return;
    }

    showLoading('Building 3D scene...');

    qposIndices = getJointQposIndices(mujoco, model);
    console.log('Joint qpos indices:', qposIndices);

    // Find free joints for scene objects (duplo, etc.)
    freeJointQposAddrs = new Map();
    for (let j = 0; j < model.njnt; j++) {
      if (model.jnt_type[j] === 0) { // mjJNT_FREE = 0
        try {
          const name = mujoco.mj_id2name(model, 3, j); // OBJ_JOINT = 3
          if (name) {
            freeJointQposAddrs.set(name, model.jnt_qposadr[j]);
            console.log(`Free joint "${name}" at qpos[${model.jnt_qposadr[j]}]`);
          }
        } catch { /* unnamed joint */ }
      }
    }

    // Remove old scene root if switching scenes
    if (sceneRoot) {
      scene.remove(sceneRoot);
      sceneRoot.traverse((obj) => {
        if ((obj as any).geometry) (obj as any).geometry.dispose();
        if ((obj as any).material) {
          const mat = (obj as any).material;
          if (Array.isArray(mat)) mat.forEach((m: any) => m.dispose());
          else mat.dispose();
        }
      });
    }

    // Remove old FOV overlay
    if (fovOverlay) {
      fovOverlay.dispose();
      fovOverlay = null;
    }

    const built = buildScene(mujoco, model, data);
    bodyGroups = built.bodyGroups;
    sceneRoot = built.root;
    scene.add(sceneRoot);

    // Create FOV overlay
    fovOverlay = new FovOverlay(mujoco, model, data, sceneRoot);

    updateBodyTransforms(model, data, bodyGroups);
    fovOverlay.update();

    hideStatus();
    ui.loadBtn.disabled = false;
  }

  await loadScene(DEFAULT_SCENE);

  // --- 3. Render Loop ---
  function renderLoop() {
    controls.update();
    renderer.render(scene, camera);
    requestAnimationFrame(renderLoop);
  }
  renderLoop();

  // --- 4. Frame Update Callback ---
  function getUnitMode(): UnitMode {
    return ui.unitsSelect.value as UnitMode;
  }

  function getDataSource(): 'action' | 'state' {
    return ui.dataSourceSelect.value as 'action' | 'state';
  }

  function getFrameValues(frame: import('./types').FrameData): number[] {
    const src = getDataSource();
    if (src === 'action' && frame.action) return frame.action;
    return frame.state;
  }

  function onFrame(frameIdx: number) {
    if (!currentEpisode || frameIdx >= currentEpisode.frames.length) return;

    const frame = currentEpisode.frames[frameIdx];
    const values = getFrameValues(frame);
    const radians = convertToRadians(values, getUnitMode());
    setJointPositions(data, qposIndices, radians);

    mujoco.mj_forward(model, data);
    updateBodyTransforms(model, data, bodyGroups);

    // Update FOV overlay (wrist cam moves with arm)
    if (fovOverlay) fovOverlay.update();

    // Update chart playhead
    chart.setFrame(frameIdx);

    // Sync camera videos
    cameras.seekToFrame(frameIdx);
  }

  // --- 5. Playback Callbacks ---
  playback.setCallbacks(onFrame, (state) => {
    updatePlaybackUI(ui, state);
  });

  // Repeat button
  updateRepeatButton(ui, playback.getState().repeatMode);
  ui.repeatBtn.addEventListener('click', () => {
    playback.cycleRepeatMode();
    updateRepeatButton(ui, playback.getState().repeatMode);
  });

  // Auto-advance to next episode on repeat-all
  playback.setOnEpisodeEnd(() => {
    if (!datasetCtx) return;
    const total = datasetCtx.info.total_episodes;
    const nextIdx = (currentEpisodeIndex + 1) % total;
    loadEpisodeData(nextIdx).then(() => playback.play());
  });

  // --- 6. Wire UI Events ---

  function applySceneObjects(episode: EpisodeData) {
    if (!episode.sceneObjects) return;
    for (const [objName, info] of Object.entries(episode.sceneObjects)) {
      const jointName = `${objName}_joint`;
      const qposAddr = freeJointQposAddrs.get(jointName);
      if (qposAddr === undefined) continue;

      data.qpos[qposAddr + 0] = info.position.x;
      data.qpos[qposAddr + 1] = info.position.y;
      data.qpos[qposAddr + 2] = info.position.z;
      if (info.quaternion) {
        data.qpos[qposAddr + 3] = info.quaternion.w;
        data.qpos[qposAddr + 4] = info.quaternion.x;
        data.qpos[qposAddr + 5] = info.quaternion.y;
        data.qpos[qposAddr + 6] = info.quaternion.z;
      }
      console.log(`Set ${objName} position: [${info.position.x}, ${info.position.y}, ${info.position.z}]`);
    }
    mujoco.mj_forward(model, data);
    updateBodyTransforms(model, data, bodyGroups);
    if (fovOverlay) fovOverlay.update();
  }

  async function loadEpisodeData(episodeIdx: number) {
    if (!datasetCtx) return;
    showLoading(`Loading episode ${episodeIdx}...`);

    try {
      currentEpisode = await loadEpisode(datasetCtx, episodeIdx);

      // Auto-switch scene if episode specifies a different scene_xml
      if (currentEpisode.sceneXml && currentEpisode.sceneXml !== currentSceneFile) {
        // Check if the scene file is one of our known scenes
        const knownScene = SCENES.find(s => s.file === currentEpisode!.sceneXml);
        if (knownScene) {
          console.log(`[auto-scene] Switching to "${knownScene.label}" (${knownScene.file}) for episode ${episodeIdx}`);
          playback.pause();
          await loadScene(knownScene.file);
          ui.sceneSelect.value = knownScene.file;
        } else {
          console.warn(`[auto-scene] Episode ${episodeIdx} requires unknown scene "${currentEpisode.sceneXml}", keeping current scene`);
        }
      }

      // Apply scene object positions (block, bowl, etc.) before playback
      applySceneObjects(currentEpisode);

      // Log first frame for debugging
      if (currentEpisode.frames.length > 0) {
        const f0 = currentEpisode.frames[0];
        console.log(`Episode ${episodeIdx}: ${currentEpisode.totalFrames} frames, ${currentEpisode.videos.length} cameras`);
        console.log('  Frame 0 state:', f0.state);
        console.log('  Frame 0 action:', f0.action);
      }

      currentEpisodeIndex = episodeIdx;
      const decision = reviewContext?.decisions?.[episodeIdx.toString()];
      curationState.textContent = decision ? decision.replace('_', ' ') : 'Not reviewed';
      curationNote.value = '';
      playback.loadEpisode(currentEpisode);
      chart.loadEpisode(currentEpisode);
      cameras.loadVideos(currentEpisode.videos, currentEpisode.fps, currentEpisode.totalFrames);
      selectEpisodeInList(ui, episodeIdx);
      hideStatus();
      updateUrlParams(datasetCtx.repoId, datasetCtx.revision, episodeIdx);
    } catch (err) {
      showError(`Failed to load episode: ${err}`);
      console.error(err);
    }
  }

  async function loadDataset(repoId: string, episodeIdx?: number, revision?: string) {
    showLoading(`Loading dataset: ${repoId}...`);
    ui.loadBtn.disabled = true;

    try {
      datasetCtx = await loadDatasetContext(repoId, revision);
      const info = datasetCtx.info;

      populateEpisodes(ui, info.total_episodes, (idx) => loadEpisodeData(idx));
      setDatasetMeta(ui, info.fps, info.total_episodes, info.total_frames);

      const ep = episodeIdx !== undefined && episodeIdx < info.total_episodes ? episodeIdx : 0;
      await loadEpisodeData(ep);
    } catch (err) {
      showError(`Failed to load dataset: ${err}`);
      console.error(err);
    } finally {
      ui.loadBtn.disabled = false;
    }
  }

  async function loadUploadedDataset(source: NonNullable<ReviewContext['source']>) {
    showLoading(`Loading dataset: ${source.label}...`);
    ui.loadBtn.disabled = true;
    try {
      datasetCtx = await loadUploadedDatasetContext(
        source.label,
        source.revision,
        source.info,
        source.fileBaseUrl,
      );
      populateEpisodes(ui, source.info.total_episodes, (idx) => loadEpisodeData(idx));
      setDatasetMeta(ui, source.info.fps, source.info.total_episodes, source.info.total_frames);
      await loadEpisodeData(0);
    } catch (error) {
      showError(`Failed to load uploaded dataset: ${error}`);
    } finally {
      ui.loadBtn.disabled = false;
    }
  }

  const fovToggle = document.getElementById('fov-toggle') as HTMLInputElement;
  fovToggle.addEventListener('change', () => {
    if (fovOverlay) fovOverlay.setVisible(fovToggle.checked);
  });

  ui.sceneSelect.addEventListener('change', async () => {
    playback.pause();
    await loadScene(ui.sceneSelect.value);
    // Re-apply current episode data if loaded
    if (currentEpisode) {
      applySceneObjects(currentEpisode);
      onFrame(playback.getState().currentFrame);
    }
  });

  ui.loadBtn.addEventListener('click', () => {
    const repoId = ui.datasetInput.value.trim();
    if (repoId) loadDataset(repoId);
  });

  ui.datasetInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      const repoId = ui.datasetInput.value.trim();
      if (repoId) loadDataset(repoId);
    }
  });

  ui.dataSourceSelect.addEventListener('change', () => {
    chart.setDataSource(getDataSource());
    const state = playback.getState();
    onFrame(state.currentFrame);
  });

  ui.unitsSelect.addEventListener('change', () => {
    chart.setUnitMode(getUnitMode());
    const state = playback.getState();
    onFrame(state.currentFrame);
  });

  ui.playPauseBtn.addEventListener('click', () => playback.togglePlay());
  ui.prevFrameBtn.addEventListener('click', () => playback.prevFrame());
  ui.nextFrameBtn.addEventListener('click', () => playback.nextFrame());

  ui.timeline.addEventListener('input', () => {
    const frame = parseInt(ui.timeline.value, 10);
    playback.seek(frame);
  });

  ui.speedSelect.addEventListener('change', () => {
    playback.setSpeed(parseFloat(ui.speedSelect.value));
  });

  window.addEventListener('keydown', (e) => {
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement) return;

    switch (e.code) {
      case 'Space':
        e.preventDefault();
        playback.togglePlay();
        break;
      case 'ArrowLeft':
        e.preventDefault();
        playback.prevFrame();
        break;
      case 'ArrowRight':
        e.preventDefault();
        playback.nextFrame();
        break;
      case 'Home':
        e.preventDefault();
        playback.seek(0);
        break;
      case 'End':
        e.preventDefault();
        if (currentEpisode) playback.seek(currentEpisode.totalFrames - 1);
        break;
    }
  });

  // --- 7. Auto-load ---
  const repoToLoad = urlDataset || DEFAULT_DATASET;
  await loadDataset(repoToLoad, urlEpisode, urlRevision);

  // Landing page: ready to enter
  if (hasDatasetParam) {
    // Auto-dismiss when loaded via URL param
    dismissLanding();
  } else {
    showLandingReady();
  }
}

main().catch(console.error);
