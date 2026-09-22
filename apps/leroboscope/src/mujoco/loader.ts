import type { MuJoCoModule } from '../types';
import { MESH_FILES } from '../constants';

// Cache the WASM module so we only load it once
let cachedMujoco: MuJoCoModule | null = null;
let meshesLoaded = false;

/**
 * Initialize MuJoCo WASM, load MJCF + STL files into the virtual filesystem,
 * and return model + data.
 */
export async function initMuJoCo(sceneFile: string): Promise<{
  mujoco: MuJoCoModule;
  model: InstanceType<MuJoCoModule['MjModel']> & Record<string, any>;
  data: InstanceType<MuJoCoModule['MjData']> & Record<string, any>;
}> {
  // Load WASM module once
  if (!cachedMujoco) {
    const loadMuJoCo = (await import('mujoco-js')).default;
    cachedMujoco = await loadMuJoCo();

    // Create directories in the Emscripten virtual filesystem
    cachedMujoco.FS.mkdir('/scenes');
    cachedMujoco.FS.mkdir('/scenes/meshes');
  }

  const mujoco = cachedMujoco;

  // Load meshes once (shared by all scenes)
  if (!meshesLoaded) {
    const meshPromises = MESH_FILES.map(async (filename) => {
      const response = await fetch(`scenes/meshes/${filename}`);
      const buffer = await response.arrayBuffer();
      mujoco.FS.writeFile(`/scenes/meshes/${filename}`, new Uint8Array(buffer));
    });
    await Promise.all(meshPromises);
    meshesLoaded = true;
  }

  // Fetch and write the MJCF scene file
  const xmlResponse = await fetch(`scenes/${sceneFile}`);
  const xmlText = await xmlResponse.text();
  const vfsPath = `/scenes/${sceneFile}`;

  // Overwrite if already exists
  try { mujoco.FS.unlink(vfsPath); } catch { /* doesn't exist yet */ }
  mujoco.FS.writeFile(vfsPath, xmlText);

  // Load model from MJCF
  const model = mujoco.MjModel.loadFromXML(vfsPath) as any;
  const data = new mujoco.MjData(model) as any;

  // Run forward kinematics to initialize body positions
  mujoco.mj_forward(model, data);

  return { mujoco, model, data };
}
