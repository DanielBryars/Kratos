//! The project a single-project deployment runs in.
//!
//! Everything Kratos owns now belongs to a project, but nothing yet creates a second one: the
//! console has no project switcher and registration has no project-scoped pairing credential. So
//! there is exactly one row, created by the projects migration, and every path that needs a
//! project without being told one names it here.
//!
//! It lives in its own module because it was previously declared twice, in `operator` and in
//! `registry`, with two different comments explaining two different reasons. Two copies of one
//! magic UUID is one copy too many to keep honest, and a test fixture needs the same value again.

use uuid::Uuid;

/// The project every existing resource was migrated into, the one a first operator joins, and the
/// one an interactive registration enters.
///
/// The literal matches the row `202609200026_projects_and_invitations.sql` inserts. It is a
/// version-4-shaped constant rather than a generated value precisely so that the migration and
/// the code can name the same project without the code having to look it up.
///
/// Deliberately **not** a column default. A default would put a row in this project whenever a
/// caller forgot to say, which is the exact failure the NOT NULL is there to catch: the point of
/// the constraint is that a path with no project is a bug, not a row quietly filed here.
pub const DEFAULT_PROJECT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_d00f);
