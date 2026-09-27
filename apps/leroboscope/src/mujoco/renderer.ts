import * as THREE from 'three';
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js';

/**
 * Create the Three.js scene, camera, renderer, lights, and orbit controls.
 */
export function createRenderer(container: HTMLElement): {
  scene: THREE.Scene;
  camera: THREE.PerspectiveCamera;
  renderer: THREE.WebGLRenderer;
  controls: OrbitControls;
} {
  // Scene
  const scene = new THREE.Scene();
  scene.background = new THREE.Color(0x1a1a2e);

  // Apply Z-up to Y-up coordinate transform at scene root
  // MuJoCo: +X right, +Y forward, +Z up
  // Three.js: +X right, +Y up, -Z forward
  // Rotation: -90 degrees around X axis
  scene.rotation.x = -Math.PI / 2;

  // Camera
  const aspect = container.clientWidth / container.clientHeight;
  const camera = new THREE.PerspectiveCamera(45, aspect, 0.01, 100);
  // Position camera to look at the robot from a nice angle (in Y-up coordinates)
  camera.position.set(0.5, 0.5, 0.5);
  camera.lookAt(0.1, 0.1, 0);

  // Renderer
  const renderer = new THREE.WebGLRenderer({ antialias: true, logarithmicDepthBuffer: true });
  renderer.setSize(container.clientWidth, container.clientHeight);
  renderer.setPixelRatio(window.devicePixelRatio);
  renderer.shadowMap.enabled = true;
  renderer.shadowMap.type = THREE.PCFShadowMap;
  renderer.toneMapping = THREE.ACESFilmicToneMapping;
  renderer.toneMappingExposure = 1.2;
  container.appendChild(renderer.domElement);

  // Lights (in Y-up space, outside the scene rotation)
  const ambientLight = new THREE.AmbientLight(0xffffff, 0.5);
  // Add to the scene but note the rotation will apply
  scene.add(ambientLight);

  const dirLight = new THREE.DirectionalLight(0xffffff, 1.0);
  dirLight.position.set(0, 0, 3.5); // Z-up position (will be rotated by scene)
  dirLight.castShadow = true;
  dirLight.shadow.mapSize.set(2048, 2048);
  dirLight.shadow.camera.near = 0.1;
  dirLight.shadow.camera.far = 10;
  dirLight.shadow.camera.left = -2;
  dirLight.shadow.camera.right = 2;
  dirLight.shadow.camera.top = 2;
  dirLight.shadow.camera.bottom = -2;
  scene.add(dirLight);

  const fillLight = new THREE.DirectionalLight(0xffffff, 0.3);
  fillLight.position.set(-1, -1, 2);
  scene.add(fillLight);

  // Orbit Controls
  const controls = new OrbitControls(camera, renderer.domElement);
  controls.enableDamping = true;
  controls.dampingFactor = 0.1;
  controls.target.set(0.15, 0, 0.1); // Look at robot base area (Z-up, but camera is in Y-up)
  // Actually set target in Y-up space since the camera is not inside the scene rotation
  controls.target.set(0.15, 0.1, 0);
  controls.update();

  // Handle resize
  const onResize = () => {
    const w = container.clientWidth;
    const h = container.clientHeight;
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
    renderer.setSize(w, h);
  };
  window.addEventListener('resize', onResize);

  return { scene, camera, renderer, controls };
}
