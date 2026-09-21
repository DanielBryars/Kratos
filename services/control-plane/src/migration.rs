use sqlx::PgPool;

/// Grants the application identity data access to all current and future public-schema objects.
///
/// The caller must be the migration owner. Identifier quoting is delegated to `PostgreSQL` so an IAM
/// database username containing punctuation cannot change the statements' structure.
///
/// # Errors
///
/// Returns the underlying `PostgreSQL` error when quoting, a grant, or the transaction fails.
pub async fn grant_application_privileges(
    pool: &PgPool,
    application_user: &str,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let quoted_user: String = sqlx::query_scalar("SELECT quote_ident($1)")
        .bind(application_user)
        .fetch_one(&mut *transaction)
        .await?;
    let quoted_database: String = sqlx::query_scalar("SELECT quote_ident(current_database())")
        .fetch_one(&mut *transaction)
        .await?;
    let statements = [
        format!("GRANT CONNECT ON DATABASE {quoted_database} TO {quoted_user}"),
        format!("GRANT USAGE ON SCHEMA public TO {quoted_user}"),
        format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {quoted_user}"
        ),
        format!("GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO {quoted_user}"),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {quoted_user}"
        ),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO {quoted_user}"
        ),
    ];
    for statement in statements {
        sqlx::query(&statement).execute(&mut *transaction).await?;
    }
    transaction.commit().await
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::grant_application_privileges;

    #[sqlx::test(migrations = "./migrations")]
    async fn application_role_can_modify_current_and_future_tables(pool: PgPool) {
        let role = format!("runtime.{}@example", Uuid::new_v4().simple());
        let quoted_role: String = sqlx::query_scalar("SELECT quote_ident($1)")
            .bind(&role)
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE ROLE {quoted_role}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE grant_test_existing (id integer PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();

        grant_application_privileges(&pool, &role).await.unwrap();
        sqlx::query("CREATE TABLE grant_test_future (id integer PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();

        let mut connection = pool.acquire().await.unwrap();
        sqlx::query(&format!("SET ROLE {quoted_role}"))
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO grant_test_existing VALUES (1)")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("UPDATE grant_test_existing SET id = 2 WHERE id = 1")
            .execute(&mut *connection)
            .await
            .unwrap();
        let value: i32 = sqlx::query_scalar("SELECT id FROM grant_test_existing")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        assert_eq!(value, 2);
        sqlx::query("DELETE FROM grant_test_existing WHERE id = 2")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO grant_test_future VALUES (1)")
            .execute(&mut *connection)
            .await
            .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    #[allow(clippy::too_many_lines)]
    async fn artifact_storage_upgrade_rejects_unprovable_verified_rows(pool: PgPool) {
        let schema = format!("upgrade_{}", Uuid::new_v4().simple());
        let mut transaction = pool.begin().await.unwrap();
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query(&format!("SET LOCAL search_path TO {schema}"))
            .execute(&mut *transaction)
            .await
            .unwrap();
        for migration in [
            include_str!("../migrations/202609190001_worker_registry.sql"),
            include_str!("../migrations/202609190002_worker_observation_time.sql"),
            include_str!("../migrations/202609190003_human_roles.sql"),
            include_str!("../migrations/202609190004_worker_registration_requests.sql"),
            include_str!("../migrations/202609190005_job_queue.sql"),
            include_str!("../migrations/202609200001_one_active_attempt_per_job.sql"),
            include_str!("../migrations/202609200010_job_artifacts.sql"),
        ] {
            sqlx::raw_sql(migration)
                .execute(&mut *transaction)
                .await
                .unwrap();
        }

        // No project_id on these rows: this test replays the schema as it stood at
        // 202609200010, before the projects migration added the column. Adding it here
        // would test a schema that never existed.
        let owner_id = Uuid::new_v4();
        let worker_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        let requirement_id = Uuid::new_v4();
        let manifest_id = Uuid::new_v4();
        let artifact_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
             VALUES ($1, 'test', $2, 'Owner')",
        )
        .bind(owner_id)
        .bind(owner_id.to_string())
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workers \
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, capabilities) \
             VALUES ($1, $2, $3, 'Worker', '1.1', 'busy', '{}'::jsonb)",
        )
        .bind(worker_id)
        .bind(owner_id)
        .bind(Uuid::new_v4())
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO jobs \
             (id, owner_identity_id, name, image_reference, timeout_seconds, status, assigned_worker_id) \
             VALUES ($1, $2, 'Job', $3, 120, 'assigned', $4)",
        )
        .bind(job_id)
        .bind(owner_id)
        .bind(format!("example.test/job@sha256:{}", "a".repeat(64)))
        .bind(worker_id)
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_id, attempt_number, worker_id, lease_expires_at) \
             VALUES ($1, $2, 1, $3, now() + interval '10 minutes')",
        )
        .bind(attempt_id)
        .bind(job_id)
        .bind(worker_id)
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_output_requirements \
             (id, job_id, logical_path, role, media_type, mandatory, max_bytes) \
             VALUES ($1, $2, 'model.pt', 'checkpoint', 'application/octet-stream', true, 1024)",
        )
        .bind(requirement_id)
        .bind(job_id)
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_artifact_manifests (id, attempt_id, job_id) VALUES ($1, $2, $3)",
        )
        .bind(manifest_id)
        .bind(attempt_id)
        .bind(job_id)
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_artifacts \
             (id, manifest_id, attempt_id, job_id, output_requirement_id, logical_path, role, \
              media_type, mandatory, byte_length, sha256, crc32c, object_key, status, \
              storage_generation, uploaded_byte_length, uploaded_crc32c, upload_started_at, \
              upload_completed_at, verified_storage_generation, verified_byte_length, \
              verified_crc32c, verification_source, verified_at) \
             VALUES ($1, $2, $3, $4, $5, 'model.pt', 'checkpoint', 'application/octet-stream', \
                     true, 512, $6, 'ImIEBA==', $7, 'verified', 42, 512, 'ImIEBA==', now(), \
                     now(), 42, 512, 'ImIEBA==', 'gcs_metadata', now())",
        )
        .bind(artifact_id)
        .bind(manifest_id)
        .bind(attempt_id)
        .bind(job_id)
        .bind(requirement_id)
        .bind("b".repeat(64))
        .bind(format!("v1/owners/{owner_id}/artifacts/{artifact_id}"))
        .execute(&mut *transaction)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!(
            "../migrations/202609200021_artifact_storage.sql"
        ))
        .execute(&mut *transaction)
        .await
        .unwrap();
        let upgraded: (
            String,
            Option<String>,
            Option<String>,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT status, state_reason, verified_sha256, verified_at \
                 FROM job_artifacts WHERE id = $1",
        )
        .bind(artifact_id)
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
        assert_eq!(upgraded.0, "rejected");
        assert_eq!(
            upgraded.1.as_deref(),
            Some("storage_identity_requires_reissue")
        );
        assert!(upgraded.2.is_none());
        assert!(upgraded.3.is_none());
        transaction.rollback().await.unwrap();
    }
}
