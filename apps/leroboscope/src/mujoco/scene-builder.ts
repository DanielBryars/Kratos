import * as THREE from 'three';
import type { MuJoCoModule } from '../types';
import { GEOM_TYPES } from '../constants';

/**
 * Build Three.js meshes from MuJoCo model geoms.
 * Returns a map of body index -> Three.js Group, and an array of geom meshes for materials.
 *
 * Pattern ported from zalo/mujoco_wasm mujocoUtils.js
 */
export function buildScene(
  mujoco: MuJoCoModule,
  model: any,
  data: any,
): {
  bodyGroups: Map<number, THREE.Group>;
  root: THREE.Group;
} {
  const root = new THREE.Group();
  const bodyGroups = new Map<number, THREE.Group>();

  // Create a group for each body
  for (let b = 0; b < model.nbody; b++) {
    const group = new THREE.Group();
    group.name = `body_${b}`;
    try {
      const name = mujoco.mj_id2name(model, 1, b); // OBJ_BODY = 1
      if (name) group.name = name;
    } catch { /* unnamed body */ }
    bodyGroups.set(b, group);
    root.add(group);
  }

  // Walk all geoms and create Three.js geometry
  for (let g = 0; g < model.ngeom; g++) {
    const geomType = model.geom_type[g];
    const bodyId = model.geom_bodyid[g];
    const group = bodyGroups.get(bodyId);
    if (!group) continue;

    // Check visibility group - skip group 3 (collision-only)
    const geomGroup = model.geom_group[g];
    if (geomGroup === 3) continue;

    // Get geom size (3 values)
    const sx = model.geom_size[g * 3 + 0];
    const sy = model.geom_size[g * 3 + 1];
    const sz = model.geom_size[g * 3 + 2];

    // Get geom color
    const r = model.geom_rgba[g * 4 + 0];
    const gCol = model.geom_rgba[g * 4 + 1];
    const b = model.geom_rgba[g * 4 + 2];
    const a = model.geom_rgba[g * 4 + 3];

    // Check if geom has a material
    const matId = model.geom_matid[g];
    let color: THREE.Color;
    let opacity = a;

    if (matId >= 0) {
      // Use material RGBA
      color = new THREE.Color(
        model.mat_rgba[matId * 4 + 0],
        model.mat_rgba[matId * 4 + 1],
        model.mat_rgba[matId * 4 + 2],
      );
      opacity = model.mat_rgba[matId * 4 + 3];
    } else {
      color = new THREE.Color(r, gCol, b);
    }

    let geometry: THREE.BufferGeometry | null = null;
    let mesh: THREE.Mesh | null = null;

    switch (geomType) {
      case GEOM_TYPES.PLANE: {
        // MuJoCo plane faces local Z; PlaneGeometry also faces Z; root rotation handles Z-up -> Y-up
        geometry = new THREE.PlaneGeometry(10, 10);
        // Create checker texture matching the MJCF groundplane material
        const checkerCanvas = document.createElement('canvas');
        checkerCanvas.width = 512;
        checkerCanvas.height = 512;
        const checkerCtx = checkerCanvas.getContext('2d')!;
        const gridSize = 32;
        for (let ci = 0; ci < checkerCanvas.width / gridSize; ci++) {
          for (let cj = 0; cj < checkerCanvas.height / gridSize; cj++) {
            // rgb1="0.2 0.3 0.4" rgb2="0.1 0.2 0.3"
            checkerCtx.fillStyle = (ci + cj) % 2 === 0 ? '#334d66' : '#1a334d';
            checkerCtx.fillRect(ci * gridSize, cj * gridSize, gridSize, gridSize);
          }
        }
        const checkerTex = new THREE.CanvasTexture(checkerCanvas);
        checkerTex.wrapS = THREE.RepeatWrapping;
        checkerTex.wrapT = THREE.RepeatWrapping;
        checkerTex.repeat.set(5, 5);
        const planeMat = new THREE.MeshStandardMaterial({
          map: checkerTex,
          roughness: 0.8,
          side: THREE.DoubleSide,
        });
        mesh = new THREE.Mesh(geometry, planeMat);
        break;
      }

      case GEOM_TYPES.SPHERE: {
        geometry = new THREE.SphereGeometry(sx, 24, 16);
        break;
      }

      case GEOM_TYPES.CAPSULE: {
        // sx = radius, sy (or sz) = half-length
        geometry = new THREE.CapsuleGeometry(sx, sz * 2, 8, 16);
        // Three.js capsule is Y-aligned, MuJoCo is Z-aligned
        geometry.rotateX(Math.PI / 2);
        break;
      }

      case GEOM_TYPES.CYLINDER: {
        // sx = radius, sz = half-length
        geometry = new THREE.CylinderGeometry(sx, sx, sz * 2, 24);
        // Three.js cylinder is Y-aligned, MuJoCo is Z-aligned
        geometry.rotateX(Math.PI / 2);
        break;
      }

      case GEOM_TYPES.BOX: {
        geometry = new THREE.BoxGeometry(sx * 2, sy * 2, sz * 2);
        break;
      }

      case GEOM_TYPES.MESH: {
        const meshId = model.geom_dataid[g];
        if (meshId < 0) continue;
        geometry = buildMeshGeometry(model, meshId);
        break;
      }

      case GEOM_TYPES.ELLIPSOID: {
        geometry = new THREE.SphereGeometry(1, 24, 16);
        // Scale to ellipsoid dimensions
        geometry.scale(sx, sy, sz);
        break;
      }

      default:
        continue;
    }

    if (!mesh && geometry) {
      const material = new THREE.MeshStandardMaterial({
        color,
        roughness: 0.6,
        metalness: 0.1,
        transparent: opacity < 1,
        opacity,
        side: geomType === GEOM_TYPES.PLANE ? THREE.DoubleSide : THREE.FrontSide,
      });
      mesh = new THREE.Mesh(geometry, material);
    }

    if (mesh) {
      // Set local position within body (geom_pos is relative to body)
      const px = model.geom_pos[g * 3 + 0];
      const py = model.geom_pos[g * 3 + 1];
      const pz = model.geom_pos[g * 3 + 2];
      mesh.position.set(px, py, pz);

      // Set local orientation (geom_quat is [w,x,y,z])
      const qw = model.geom_quat[g * 4 + 0];
      const qx = model.geom_quat[g * 4 + 1];
      const qy = model.geom_quat[g * 4 + 2];
      const qz = model.geom_quat[g * 4 + 3];
      mesh.quaternion.set(qx, qy, qz, qw);

      mesh.castShadow = true;
      mesh.receiveShadow = true;
      mesh.name = `geom_${g}`;

      group.add(mesh);
    }
  }

  return { bodyGroups, root };
}

/**
 * Build a Three.js BufferGeometry from a MuJoCo mesh asset.
 */
function buildMeshGeometry(model: any, meshId: number): THREE.BufferGeometry {
  const geometry = new THREE.BufferGeometry();

  // Get mesh data offsets
  const vertStart = model.mesh_vertadr[meshId];
  const vertCount = model.mesh_vertnum[meshId];
  const faceStart = model.mesh_faceadr[meshId];
  const faceCount = model.mesh_facenum[meshId];

  // Extract vertices
  const vertices = new Float32Array(vertCount * 3);
  for (let i = 0; i < vertCount * 3; i++) {
    vertices[i] = model.mesh_vert[vertStart * 3 + i];
  }

  // Extract normals
  const normals = new Float32Array(vertCount * 3);
  for (let i = 0; i < vertCount * 3; i++) {
    normals[i] = model.mesh_normal[vertStart * 3 + i];
  }

  // Extract face indices
  const indices = new Uint32Array(faceCount * 3);
  for (let i = 0; i < faceCount * 3; i++) {
    indices[i] = model.mesh_face[faceStart * 3 + i];
  }

  geometry.setAttribute('position', new THREE.BufferAttribute(vertices, 3));
  geometry.setAttribute('normal', new THREE.BufferAttribute(normals, 3));
  geometry.setIndex(new THREE.BufferAttribute(indices, 1));

  return geometry;
}

/**
 * Update body group world transforms from MuJoCo data (xpos, xquat).
 * Call this each frame after mj_forward.
 *
 * MuJoCo is Z-up, Three.js is Y-up.
 * We apply the coordinate transform at the root level.
 */
export function updateBodyTransforms(
  model: any,
  data: any,
  bodyGroups: Map<number, THREE.Group>,
): void {
  for (let b = 0; b < model.nbody; b++) {
    const group = bodyGroups.get(b);
    if (!group) continue;

    // xpos: world position of body
    const px = data.xpos[b * 3 + 0];
    const py = data.xpos[b * 3 + 1];
    const pz = data.xpos[b * 3 + 2];

    // xquat: world orientation [w,x,y,z]
    const qw = data.xquat[b * 4 + 0];
    const qx = data.xquat[b * 4 + 1];
    const qy = data.xquat[b * 4 + 2];
    const qz = data.xquat[b * 4 + 3];

    group.position.set(px, py, pz);
    group.quaternion.set(qx, qy, qz, qw);
  }
}
