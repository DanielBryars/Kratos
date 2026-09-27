import { SIM_ACTION_LOW, SIM_ACTION_HIGH, GRIPPER_IDX, NUM_JOINTS } from '../constants';

export type UnitMode = 'degrees' | 'normalized' | 'radians';

/**
 * Convert values to radians based on the selected unit mode.
 */
export function convertToRadians(values: number[], mode: UnitMode): number[] {
  switch (mode) {
    case 'degrees':
      return degreesToRadians(values);
    case 'normalized':
      return normalizedToRadians(values);
    case 'radians':
      return clipToLimits(values);
  }
}

/**
 * Degrees absolute for joints, 0-100 for gripper.
 * Joints: degrees -> radians (multiply by π/180)
 * Gripper: [0, 100] -> [LOW, HIGH]
 */
function degreesToRadians(values: number[]): number[] {
  const radians = new Array(NUM_JOINTS);
  for (let i = 0; i < NUM_JOINTS; i++) {
    if (i === GRIPPER_IDX) {
      // Gripper: [0, 100] -> [LOW, HIGH]
      const t = values[i] / 100.0;
      radians[i] = SIM_ACTION_LOW[i] + t * (SIM_ACTION_HIGH[i] - SIM_ACTION_LOW[i]);
    } else {
      // Joints: degrees -> radians
      radians[i] = values[i] * (Math.PI / 180);
    }
  }
  return clipToLimits(radians);
}

/**
 * Normalized: joints [-100, 100], gripper [0, 100].
 */
function normalizedToRadians(normalized: number[]): number[] {
  const radians = new Array(NUM_JOINTS);
  for (let i = 0; i < NUM_JOINTS; i++) {
    let t: number;
    if (i === GRIPPER_IDX) {
      t = normalized[i] / 100.0;
    } else {
      t = (normalized[i] + 100.0) / 200.0;
    }
    radians[i] = SIM_ACTION_LOW[i] + t * (SIM_ACTION_HIGH[i] - SIM_ACTION_LOW[i]);
  }
  return radians;
}

/**
 * Clip joint values to valid range.
 */
function clipToLimits(radians: number[]): number[] {
  return radians.map((v, i) =>
    Math.max(SIM_ACTION_LOW[i], Math.min(SIM_ACTION_HIGH[i], v))
  );
}

/**
 * Map joint names to qpos indices using model.jnt_qposadr.
 */
export function getJointQposIndices(mujoco: any, model: any): number[] {
  const MOTOR_NAMES = [
    'shoulder_pan', 'shoulder_lift', 'elbow_flex',
    'wrist_flex', 'wrist_roll', 'gripper',
  ];

  const indices: number[] = [];
  for (const name of MOTOR_NAMES) {
    const jntId = mujoco.mj_name2id(model, 3, name); // OBJ_JOINT = 3
    if (jntId >= 0) {
      indices.push(model.jnt_qposadr[jntId]);
    } else {
      console.warn(`Joint not found: ${name}`);
      indices.push(-1);
    }
  }
  return indices;
}

/**
 * Set qpos values for the robot joints.
 */
export function setJointPositions(
  data: any,
  qposIndices: number[],
  radians: number[],
): void {
  for (let i = 0; i < radians.length && i < qposIndices.length; i++) {
    const idx = qposIndices[i];
    if (idx >= 0) {
      data.qpos[idx] = radians[i];
    }
  }
}
