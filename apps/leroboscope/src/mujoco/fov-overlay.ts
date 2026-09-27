import * as THREE from 'three';
import { OBJ_TYPES } from '../constants';

/**
 * Camera FOV projection overlay.
 * Projects each camera's frustum onto the table plane (z=TABLE_Z)
 * and draws the outline + semi-transparent fill.
 *
 * Ported from lerobot-thesis/scripts/recording/teleop_sim_vr.py
 */

const TABLE_Z = 0.015; // Table surface height in MuJoCo coords

interface CameraOverlay {
  name: string;
  camId: number;
  outlineColor: THREE.Color;
  fillColor: THREE.Color;
  fillOpacity: number;
  outlineLine: THREE.Line;
  fillMesh: THREE.Mesh;
}

export class FovOverlay {
  private mujoco: any;
  private model: any;
  private data: any;
  private parent: THREE.Group;
  private overlays: CameraOverlay[] = [];

  constructor(mujoco: any, model: any, data: any, parent: THREE.Group) {
    this.mujoco = mujoco;
    this.model = model;
    this.data = data;
    this.parent = parent;

    this.initCameras();
  }

  private initCameras(): void {
    // Discover cameras in the model
    const cameraConfigs: { name: string; outlineRgb: [number, number, number]; fillRgb: [number, number, number]; fillOpacity: number }[] = [];

    for (let c = 0; c < this.model.ncam; c++) {
      let name = `cam_${c}`;
      try {
        const n = this.mujoco.mj_id2name(this.model, OBJ_TYPES.CAMERA, c);
        if (n) name = n;
      } catch { /* unnamed */ }

      if (name.includes('wrist')) {
        cameraConfigs.push({ name, outlineRgb: [1.0, 0.15, 0.15], fillRgb: [1.0, 0.1, 0.1], fillOpacity: 0.15 });
      } else if (name.includes('overhead') || name.includes('rgbd')) {
        cameraConfigs.push({ name, outlineRgb: [0.2, 0.4, 1.0], fillRgb: [0.15, 0.3, 1.0], fillOpacity: 0.1 });
      }
      // Skip unknown cameras
    }

    for (const cfg of cameraConfigs) {
      const camId = this.mujoco.mj_name2id(this.model, OBJ_TYPES.CAMERA, cfg.name);
      if (camId < 0) continue;

      // Create outline geometry (LineLoop with 4 corners)
      const outlineGeo = new THREE.BufferGeometry();
      outlineGeo.setAttribute('position', new THREE.Float32BufferAttribute(new Float32Array(4 * 3), 3));
      const outlineMat = new THREE.LineBasicMaterial({
        color: new THREE.Color(...cfg.outlineRgb),
        linewidth: 2,
      });
      const outlineLine = new THREE.LineLoop(outlineGeo, outlineMat);
      outlineLine.renderOrder = 1;

      // Create fill geometry (a quad)
      const fillGeo = new THREE.BufferGeometry();
      fillGeo.setAttribute('position', new THREE.Float32BufferAttribute(new Float32Array(4 * 3), 3));
      fillGeo.setIndex([0, 1, 2, 0, 2, 3]);
      const fillMat = new THREE.MeshBasicMaterial({
        color: new THREE.Color(...cfg.fillRgb),
        transparent: true,
        opacity: cfg.fillOpacity,
        side: THREE.DoubleSide,
        depthWrite: false,
      });
      const fillMesh = new THREE.Mesh(fillGeo, fillMat);
      fillMesh.renderOrder = 0;

      this.parent.add(outlineLine);
      this.parent.add(fillMesh);

      this.overlays.push({
        name: cfg.name,
        camId,
        outlineColor: new THREE.Color(...cfg.outlineRgb),
        fillColor: new THREE.Color(...cfg.fillRgb),
        fillOpacity: cfg.fillOpacity,
        outlineLine,
        fillMesh,
      });

      console.log(`[FOV] Camera "${cfg.name}" (id=${camId}, fovy=${this.model.cam_fovy[camId].toFixed(1)}°)`);
    }
  }

  setVisible(visible: boolean): void {
    for (const overlay of this.overlays) {
      overlay.outlineLine.visible = visible;
      overlay.fillMesh.visible = visible;
    }
  }

  /**
   * Update FOV projections. Call each frame after mj_forward.
   */
  update(): void {
    for (const overlay of this.overlays) {
      this.updateCamera(overlay);
    }
  }

  private updateCamera(overlay: CameraOverlay): void {
    const c = overlay.camId;

    // Camera position in world frame
    const camPos = new THREE.Vector3(
      this.data.cam_xpos[c * 3 + 0],
      this.data.cam_xpos[c * 3 + 1],
      this.data.cam_xpos[c * 3 + 2],
    );

    // Camera rotation matrix (3x3, row-major in MuJoCo)
    // cam_xmat columns: X=right, Y=up, -Z=forward
    const m = this.data.cam_xmat;
    const off = c * 9;
    const camMat = new THREE.Matrix3();
    camMat.set(
      m[off + 0], m[off + 1], m[off + 2],
      m[off + 3], m[off + 4], m[off + 5],
      m[off + 6], m[off + 7], m[off + 8],
    );

    // FOV parameters
    const fovyDeg = this.model.cam_fovy[c];
    const fovyRad = fovyDeg * Math.PI / 180;
    const aspect = 640 / 480; // 4:3 aspect ratio
    const halfFovY = fovyRad / 2;
    const halfFovX = Math.atan(Math.tan(halfFovY) * aspect);

    const tx = Math.tan(halfFovX);
    const ty = Math.tan(halfFovY);

    // Corner rays in camera frame (X=right, Y=up, -Z=forward)
    const raysCam = [
      new THREE.Vector3(-tx, -ty, -1), // bottom-left
      new THREE.Vector3( tx, -ty, -1), // bottom-right
      new THREE.Vector3( tx,  ty, -1), // top-right
      new THREE.Vector3(-tx,  ty, -1), // top-left
    ];

    // Intersect each ray with the table plane (z = TABLE_Z)
    const corners: THREE.Vector3[] = [];
    for (const rayCam of raysCam) {
      // Transform ray direction to world frame
      const rayWorld = rayCam.clone().applyMatrix3(camMat);

      // Ray-plane intersection: camPos + t * rayWorld, solve for z = TABLE_Z
      if (Math.abs(rayWorld.z) < 1e-6) {
        corners.push(new THREE.Vector3(0, 0, TABLE_Z)); // degenerate
        continue;
      }
      const t = (TABLE_Z - camPos.z) / rayWorld.z;
      if (t <= 0) {
        // Ray doesn't hit the table (pointing away) - project far
        const tFar = 2.0 / Math.max(Math.abs(rayWorld.x), Math.abs(rayWorld.y), 0.01);
        corners.push(new THREE.Vector3(
          camPos.x + tFar * rayWorld.x,
          camPos.y + tFar * rayWorld.y,
          TABLE_Z,
        ));
        continue;
      }
      corners.push(new THREE.Vector3(
        camPos.x + t * rayWorld.x,
        camPos.y + t * rayWorld.y,
        TABLE_Z,
      ));
    }

    // Update outline geometry
    const outlinePositions = overlay.outlineLine.geometry.getAttribute('position') as THREE.BufferAttribute;
    for (let i = 0; i < 4; i++) {
      outlinePositions.setXYZ(i, corners[i].x, corners[i].y, corners[i].z);
    }
    outlinePositions.needsUpdate = true;

    // Update fill geometry
    const fillPositions = overlay.fillMesh.geometry.getAttribute('position') as THREE.BufferAttribute;
    for (let i = 0; i < 4; i++) {
      fillPositions.setXYZ(i, corners[i].x, corners[i].y, corners[i].z);
    }
    fillPositions.needsUpdate = true;
    overlay.fillMesh.geometry.computeBoundingSphere();
  }

  dispose(): void {
    for (const overlay of this.overlays) {
      this.parent.remove(overlay.outlineLine);
      this.parent.remove(overlay.fillMesh);
      overlay.outlineLine.geometry.dispose();
      (overlay.outlineLine.material as THREE.Material).dispose();
      overlay.fillMesh.geometry.dispose();
      (overlay.fillMesh.material as THREE.Material).dispose();
    }
    this.overlays = [];
  }
}
