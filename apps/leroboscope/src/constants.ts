/** Motor names in order (matches MuJoCo model actuators) */
export const MOTOR_NAMES = [
  'shoulder_pan',
  'shoulder_lift',
  'elbow_flex',
  'wrist_flex',
  'wrist_roll',
  'gripper',
] as const;

/** Simulation action space lower bounds (radians) */
export const SIM_ACTION_LOW = [
  -1.91986,  // shoulder_pan
  -1.74533,  // shoulder_lift
  -1.69,     // elbow_flex
  -1.65806,  // wrist_flex
  -2.74385,  // wrist_roll
  -0.17453,  // gripper (closed)
];

/** Simulation action space upper bounds (radians) */
export const SIM_ACTION_HIGH = [
  1.91986,   // shoulder_pan
  1.74533,   // shoulder_lift
  1.69,      // elbow_flex
  1.65806,   // wrist_flex
  2.84121,   // wrist_roll
  1.74533,   // gripper (open)
];

export const GRIPPER_IDX = 5;
export const NUM_JOINTS = 6;

/** MuJoCo geom types */
export const GEOM_TYPES = {
  PLANE: 0,
  HFIELD: 1,
  SPHERE: 2,
  CAPSULE: 3,
  ELLIPSOID: 4,
  CYLINDER: 5,
  BOX: 6,
  MESH: 7,
} as const;

/** MuJoCo object types for mj_id2name */
export const OBJ_TYPES = {
  BODY: 1,
  JOINT: 3,
  GEOM: 5,
  SITE: 6,
  CAMERA: 7,
  MESH: 10,
  ACTUATOR: 18,
} as const;

/** Available scenes */
export const SCENES: { file: string; label: string }[] = [
  { file: 'so101_with_wrist_cam.xml', label: 'Wrist Cam (default)' },
  { file: 'so101_rgbd.xml', label: 'RGBD Camera' },
  { file: 'so101_dark_ground.xml', label: 'Dark Ground' },
  { file: 'so101_with_confuser.xml', label: 'With Confuser' },
  { file: 'so101_two_blocks.xml', label: 'Two Blocks (W+R)' },
  { file: 'so101_two_white_blocks.xml', label: 'Two White Blocks' },
  { file: 'so101_two_red_blocks.xml', label: 'Two Red Blocks' },
];

/** Default scene file */
export const DEFAULT_SCENE = SCENES[0].file;

/** STL mesh files needed by the scene */
export const MESH_FILES = [
  'waveshare_mounting_plate_so101_v2.stl',
  'sts3215_03a_v1.stl',
  'motor_holder_so101_base_v1.stl',
  'wrist_roll_follower_so101_v1.stl',
  'moving_jaw_so101_v1.stl',
  'base_motor_holder_so101_v1.stl',
  'upper_arm_so101_v1.stl',
  'wrist_roll_pitch_so101_v2.stl',
  'under_arm_so101_v1.stl',
  'rotation_pitch_so101_v1.stl',
  'motor_holder_so101_wrist_v1.stl',
  'sts3215_03a_no_horn_v1.stl',
  'base_so101_v2.stl',
];

/** Default HF dataset for testing */
export const DEFAULT_DATASET = 'danbhf/sim_pick_place_2pos_200ep_v2';
